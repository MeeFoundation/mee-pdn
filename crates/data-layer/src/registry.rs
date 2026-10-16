//! Node-local addressing: the pod-to-stores map pod operations resolve
//! through.

use std::{collections::HashMap, sync::RwLock};

use anyhow::{anyhow, Result};
use pdn_store::{api::Doc, NamespaceId};
use pdn_types::PodId;

use crate::pod::PodStore;

/// One pod as one identity holds it. `records` is `None` once the identity
/// departed: the membership store is then the pod's tombstone.
#[derive(Debug, Clone)]
pub(crate) struct PodBinding {
    pub(crate) membership: Doc,
    pub(crate) records: Option<Doc>,
}

impl PodBinding {
    fn store_of(&self, namespace: NamespaceId) -> Option<PodStore> {
        if self.membership.id() == namespace {
            return Some(PodStore::Membership);
        }
        self.records
            .as_ref()
            .filter(|records| records.id() == namespace)
            .map(|_records| PodStore::Records)
    }
}

/// Node-local registry of pods — pod id → its two stores. Lookups hand
/// back cloned [`Doc`]s (cheap handles), so no read guard escapes. The PMS
/// is not kept here — it lives inside its store handle, and the access book
/// holds it for classification.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    pods: RwLock<HashMap<PodId, PodBinding>>,
}

impl Registry {
    /// Register `pod` as held on `binding`'s stores. Refused for a pod
    /// already registered, and for a store another role holds: another
    /// pod's store, or the pod's other store.
    pub(crate) fn register_pod(&self, pod: PodId, binding: PodBinding) -> Result<()> {
        let mut pods = self
            .pods
            .write()
            .map_err(|_poisoned| anyhow!("pod registry lock poisoned"))?;
        if pods.contains_key(&pod) {
            return Err(anyhow!("pod {pod} is already registered"));
        }
        let namespaces = std::iter::once(binding.membership.id())
            .chain(binding.records.as_ref().map(Doc::id))
            .collect::<Vec<_>>();
        if let [membership, records] = namespaces.as_slice() {
            if membership == records {
                return Err(anyhow!(
                    "namespace {membership} cannot be both stores of pod {pod}"
                ));
            }
        }
        for namespace in namespaces {
            if let Some((other, _store)) = pod_of(&pods, namespace) {
                return Err(anyhow!(
                    "namespace {namespace} is a store of pod {other}; it is no store of pod {pod}"
                ));
            }
        }
        pods.insert(pod, binding);
        Ok(())
    }

    /// Replace `pod`'s record store: `None` turns the pod into its
    /// tombstone, a doc turns the tombstone back into the pod. Answers
    /// whether `pod` was registered.
    pub(crate) fn set_pod_records(&self, pod: PodId, records: Option<Doc>) -> Result<bool> {
        let mut pods = self
            .pods
            .write()
            .map_err(|_poisoned| anyhow!("pod registry lock poisoned"))?;
        let Some(binding) = pods.get_mut(&pod) else {
            return Ok(false);
        };
        binding.records = records;
        Ok(true)
    }

    pub(crate) fn unregister_pod(&self, pod: PodId) -> Result<Option<PodBinding>> {
        Ok(self
            .pods
            .write()
            .map_err(|_poisoned| anyhow!("pod registry lock poisoned"))?
            .remove(&pod))
    }

    pub(crate) fn pod(&self, pod: PodId) -> Result<Option<PodBinding>> {
        Ok(self.read_pods()?.get(&pod).cloned())
    }

    /// Every pod held, a tombstone included.
    pub(crate) fn pods(&self) -> Result<Vec<(PodId, PodBinding)>> {
        Ok(self
            .read_pods()?
            .iter()
            .map(|(pod, binding)| (*pod, binding.clone()))
            .collect())
    }

    /// Reverse lookup: which pod `namespace` is a store of, and which store.
    pub(crate) fn pod_of(&self, namespace: NamespaceId) -> Result<Option<(PodId, PodStore)>> {
        Ok(pod_of(&*self.read_pods()?, namespace))
    }

    fn read_pods(&self) -> Result<std::sync::RwLockReadGuard<'_, HashMap<PodId, PodBinding>>> {
        self.pods
            .read()
            .map_err(|_poisoned| anyhow!("pod registry lock poisoned"))
    }
}

fn pod_of(pods: &HashMap<PodId, PodBinding>, namespace: NamespaceId) -> Option<(PodId, PodStore)> {
    pods.iter()
        .find_map(|(pod, binding)| binding.store_of(namespace).map(|store| (*pod, store)))
}
