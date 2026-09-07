# Скрипты демонстрации

Половина ноутбука, разложенная по шагам. Порядок в именах: запускать слева направо, по одному на акт сценария (`../demo-script.md`).

Состояние между запусками живёт в файлах, а не в переменных вкладки: identity ноутбука в `tmp/mac-identity`, identity телефона — в `tmp/peer-id`, куда её кладёт шаг церемонии. Поэтому шаги можно запускать в любых вкладках и с перерывами.

| Скрипт | Что делает | Что в это время на телефоне |
| --- | --- | --- |
| `00-start.sh` | Поднимает node ноутбука, при пустом каталоге заводит identity | — |
| `preflight.sh` | Инструмент на месте, QR-путь цел, relay в адресах есть | — |
| `01-nodes.sh` | Node id и identity ноутбука | Bring the node up → Create an identity |
| `02-entries.sh` | Пишет свой entry и перечисляет их | My entries → Write → Read it |
| `03-invite.sh` | Ждёт relay, рисует QR, ждёт connection, запоминает peer'а | Read a code → Accepting an invitation to connect |
| `04-read-peer.sh` | Ждёт grant телефона и читает значение | Share claims with this peer → Grant read-only |
| `05-grant.sh` | Публикует свой grant телефону | Карточка What this peer shares with me наполняется |
| `06-withdraw.sh` | Ждёт снятия grant'а, потом его возвращения | Withdraw this grant, затем Grant read-only снова |
| `07-write.sh` | Выдаёт claim с правом записи и ждёт, что впишет телефон | Поле write a new value |
| `08-suspend.sh` | Пишет, пока приложение свёрнуто | Свернуть жестом home, потом вернуться |
| `09-restart.sh` | Гасит и поднимает node заново | Закрыть приложение свайпом, открыть |
| `phone-log.sh` | Запускает приложение и держит его stderr | Приложение перезапустится |
| `reset.sh` | Стирает каталог ноутбука и переустанавливает приложение | Приложение удаляется вместе с ключом node'ы |

Два места, где скрипт ждёт, и это нормально:

- `03-invite.sh` до полуминуты ждёт home relay. Первые секунды после подъёма node'ы код несёт только локальные адреса, и связь живёт лишь внутри одной сети.
- `04-read-peer.sh` ждёт grant и значение около десяти секунд: сначала едет запись grant'а, отдельно после неё — payload.

`06-withdraw.sh` принимает путь аргументом (по умолчанию `name`) — тот, который вы выдали с телефона.

`reset.sh` спрашивает подтверждение: он стирает каталог node'ы и приложение вместе с ключом, а это потеря единственной копии, а не очистка кэша.

## Профиль подписи живёт семь дней

Бесплатный provisioning profile действует неделю. Просроченный валит установку сообщением про embedded profile, и читается оно как ошибка настройки подписи, хотя это просто истёкший срок. `reset.sh` теперь проверяет срок до установки и, если он вышел, печатает команду пересборки:

```sh
cd pdn-app/ios && xcodebuild -workspace PDN.xcworkspace -scheme PDN \
  -configuration Release -destination 'generic/platform=iOS' -allowProvisioningUpdates build
```

`-allowProvisioningUpdates` обновляет профиль сам. Собранное лежит в `~/Library/Developer/Xcode/DerivedData/PDN-*/Build/Products/Release-iphoneos/PDN.app`, и `reset.sh` берёт оттуда самое свежее.
