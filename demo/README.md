# Скрипты демонстрации

Половина ноутбука, разложенная по шагам. Порядок в именах: запускать слева направо, по одному на акт сценария (`../demo-script.md`).

Каст — четыре node'ы, из них один телефон. На этой машине живут три процесса `pdn-node-http`: **alice** (3011) — устройство, на котором Alice заводит identity и пишет первые entry; **bob** (3012) — peer, с которым она устанавливает connection; **carol** (3013) — тот, кто не держит ничего Alice'иного и это показывает. Телефон присоединяется к identity Alice церемонией linking'а и остаётся единственным экраном в постановке.

Состояние между запусками живёт в файлах, а не в переменных вкладки: identity каждой node'ы в `tmp/<имя>-id`, вторая identity Alice — в `tmp/alice-other-id`. Поэтому шаги можно запускать в любых вкладках и с перерывами.

| Скрипт | Что делает | Что в это время на телефоне |
| --- | --- | --- |
| `00-start.sh` | Поднимает три node'ы, заводит identity каждой и вторую identity Alice | — |
| `preflight.sh` | Инструмент на месте, QR-путь цел, у каждой node'ы есть relay-адрес | — |
| `01-alice.sh` | Node id, две identity Alice, три entry и одноимённый путь под второй identity | — (телефон ещё ни к чему не присоединён) |
| `02-link-phone.sh` | Чеканит linking-payload для identity Alice и рисует его QR'ом | Read a code → A device joining an identity |
| `03-connect-bob.sh` | Bob чеканит invite и рисует его; ждёт connection у Bob'а и его же появления у ноутбука Alice | Read a code → Accepting an invitation to connect |
| `04-alice-grants.sh` | Ждёт grant Alice, читает значение на node'е Bob'а, показывает негранованный путь | Share claims with this peer → Grant read-only |
| `05-bob-grants.sh` | Bob выдаёт два claim'а, второй с правом записи; ждёт, что впишет телефон | Карточка What this peer shares with me, поле write a new value |
| `06-withdraw.sh` | Снятие и повторная выдача в обе стороны | Withdraw this grant, потом Grant read-only снова |
| `07-stand-in.sh` | Пишет с ноутбука Alice, пока телефон в авиарежиме | Авиарежим, приложение на экране |
| `08-outsider.sh` | Carol не получает ничего, рядом Bob читает granted claim | — |
| `phone-log.sh` | Запускает приложение и держит его stderr | Приложение перезапустится |
| `reset.sh` | Стирает три каталога и переустанавливает приложение | Приложение удаляется вместе с ключом node'ы |

Места, где скрипт ждёт, и это нормально:

- `02-link-phone.sh` и `03-connect-bob.sh` до полуминуты ждут home relay. Первые секунды после подъёма node'ы код несёт только локальные адреса, а телефон обычно не в сети ноутбука.
- `04-alice-grants.sh` ждёт grant и значение около десяти секунд: сначала едет запись grant'а, отдельно после неё — payload.
- `03-connect-bob.sh` вторым ожиданием ждёт периодического прохода: connection, установленный телефоном, доезжает до ноутбука Alice сам.

`06-withdraw.sh` и `07-stand-in.sh` останавливаются и ждут enter — там, где следующий шаг делает человек на телефоне.

`reset.sh` спрашивает подтверждение: он стирает каталоги трёх node'ов и приложение вместе с ключом, а это потеря единственной копии, а не очистка кэша.

Node'ы во время прогона не перезапускают. Node возвращается на свой каталог собой, но не на свой адрес — порт эфемерный, — и перезапущенная node недостижима для всех, с кем говорила, а экраны об этом сказать не могут: replica держит последнее дошедшее и возраст значения не сообщает.

## Профиль подписи живёт семь дней

Бесплатный provisioning profile действует неделю. Просроченный валит установку сообщением про embedded profile, и читается оно как ошибка настройки подписи, хотя это просто истёкший срок. `reset.sh` проверяет срок до установки и, если он вышел, печатает команду пересборки:

```sh
cd pdn-app/ios && xcodebuild -workspace PDN.xcworkspace -scheme PDN \
  -configuration Release -destination 'generic/platform=iOS' -allowProvisioningUpdates build
```

`-allowProvisioningUpdates` обновляет профиль сам. Собранное лежит в `~/Library/Developer/Xcode/DerivedData/PDN-*/Build/Products/Release-iphoneos/PDN.app`, и `reset.sh` берёт оттуда самое свежее.
