//! What the fuzzers found, replayed in every ordinary run: each input of `tests/corpus/fuzz/`
//! goes through the target it was found by, a crash input is refused without a crash, and every
//! target holds the inputs its manifest counts.

mod support;

use std::fs;

/// Every stored crash input is read again by its fuzz target's entry point and refused, without
/// a panic; every seed is read without one, and an event the event target reads writes back to
/// its bytes.
#[test]
fn stored_fuzz_inputs_replay_without_a_crash() {
    let manifest = support::manifest("fuzz").unwrap();
    assert_ne!(manifest.len(), 0, "fuzz/MANIFEST lists no target");
    for (target, count) in manifest {
        let run = kerita::fuzz::target(&target)
            .ok_or_else(|| format!("no target {target}"))
            .unwrap();
        let directory = support::corpus().join("fuzz").join(&target);
        let inputs: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(inputs.len(), count, "{target}: inputs against the manifest");
        for input in inputs {
            let data = fs::read(&input).unwrap();
            let read = run(&data);
            let name = input.file_name().unwrap().to_string_lossy().into_owned();
            if target == "event" {
                assert!(
                    kerita::fuzz::round_trips(&data),
                    "{target}/{name}: a read event writes other bytes"
                );
            }
            if name.starts_with("crash-") {
                assert!(!read, "{target}/{name}: a crash input was read");
            }
        }
    }
}
