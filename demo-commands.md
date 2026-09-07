# Команды для демонстрации

Все команды выполняются на macOS-хосте, не в devcontainer'е: телефон, подпись кода и XCFramework живут только здесь.

Переменные, на которые ссылается остальной файл. Заведите их один раз в той же вкладке терминала:

```sh
cd ~/vsprojects/mee/mee-pdn
export PDN=$PWD                     # корень воркспейса
export MAC=http://127.0.0.1:3011    # node ноутбука, его debug-поверхность
export DIR=$PDN/tmp/pdn-mac-node    # каталог, на котором node ноутбука возвращается собой
```

Ceremony-код, который читает телефон, — это не JSON, а base64url без padding'а поверх него: так его чеканит фасад `pdn-mobile`, и так же он его разбирает. Debug-поверхность HTTP отдаёт и принимает голый JSON. Две функции переводят одно в другое; завести их в той же вкладке:

```sh
# JSON на входе -> QR на экране, в той форме, в какой его ждёт телефон
pdnqr()  { base64 | tr -d '\n' | tr '+/' '-_' | tr -d '=' | qrencode -l L -s 6 -o "$1" && open "$1"; }
# код, снятый с экрана телефона -> JSON, который принимает /debug
pdnraw() { python3 -c "import base64,sys;d=sys.stdin.read().strip();sys.stdout.write(base64.urlsafe_b64decode(d+'='*(-len(d)%4)).decode())"; }
```

Идентификаторы (identity ноутбука, identity телефона как peer) появляются по ходу — держите их в `$MINE` и `$PEER`.

---

## 0. Разовая подготовка

```sh
just setup-tooling                        # nextest и прочий инструмент
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
(cd pdn-app && npm install)
brew install qrencode                     # чтобы показать телефону код с экрана ноутбука
```

## 1. Собрать фасад и положить его в приложение

Делается после всякого изменения `crates/pdn-mobile` или того, что оно тянет.

```sh
cd $PDN/pdn-sdk
just package-apple 0.1.0                  # release-статика под device и simulator + swift-биндинги
cp -R build/apple/PdnMobile.xcframework $PDN/pdn-app/modules/pdn/ios/
cp build/generated/swift/pdn_mobile.swift $PDN/pdn-app/modules/pdn/ios/
```

Проверить, что положили именно device-срез:

```sh
lipo -info $PDN/pdn-app/modules/pdn/ios/PdnMobile.xcframework/ios-arm64/libpdn_mobile.a
```

## 2. Собрать и поставить приложение на iPhone

```sh
cd $PDN/pdn-app
npx expo prebuild --clean                 # только после правки app.json или plugins/
npx expo run:ios --device --configuration Release
```

`--configuration Release` обязателен для демонстрации: debug-сборка ходит за JS на Metro, и без ноутбука в той же сети приложение не откроется вовсе. Первый запуск попросит выбрать телефон и может потребовать разблокированного устройства; подпись обновляется сама, потому что `expo run:ios` зовёт `xcodebuild -allowProvisioningUpdates`.

Телефон в списке и его идентификатор:

```sh
xcrun devicectl list devices
```

Переустановить уже собранный `.app`, не пересобирая:

```sh
xcrun devicectl device install app --device <device-id> \
  ~/Library/Developer/Xcode/DerivedData/PDN-*/Build/Products/Release-iphoneos/PDN.app
xcrun devicectl device process launch --device <device-id> org.mee.pdn.app
```

## 3. Логи с телефона

Единственный способ увидеть, что говорит runtime: `tracing` фасада пишет в stderr, а `NSLog` модуля — туда же.

```sh
xcrun devicectl device console --device <device-id> | grep -i "pdn\|iroh"
```

## 4. Node на ноутбуке — вторая сторона демонстрации

Собрать и поднять. `nohup … & disown` нужен, чтобы node пережила закрытие вкладки:

```sh
cd $PDN
cargo build -p pdn-node-http
PDN_DATA_DIR=$DIR PDN_DEBUG=1 \
  nohup ./target/debug/pdn-node-http > tmp/pdn-mac-node.log 2>&1 & disown
```

Убедиться, что поднялась, и увидеть node id и hosted identities:

```sh
curl -s $MAC/ready
curl -s $MAC/debug/status | jq
tail -f $PDN/tmp/pdn-mac-node.log
```

Погасить:

```sh
pkill -f pdn-node-http
```

Каталог `$DIR` — это вся durable-часть: node key, replicas, payloads. Стереть его значит потерять identity ноутбука безвозвратно; для чистого прогона демонстрации это как раз то, что нужно:

```sh
pkill -f pdn-node-http && rm -rf $DIR
```

## 5. Действия ноутбука через `/debug`

Поверхность — строительные леса, не контракт. Она нужна затем, чтобы ноутбук делал ровно те же акты, что человек делает пальцем на экране.

### Identity

```sh
# Завести identity; запомнить её — все остальные вызовы адресованы ей
export MINE=$(curl -s -X POST $MAC/debug/identities | jq -r .identity)
echo $MINE > tmp/pdn-mac-identity

# Какие identity эта node хостит
curl -s $MAC/debug/identities | jq
```

### Connection

```sh
# Отчеканить invite (lifetime_secs необязателен, по умолчанию — короткий default runtime'а)
curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=120" > tmp/invite.json

# Показать его телефону как QR: телефон читает под актом «Accepting an invitation to connect»
pdnqr tmp/invite.png < tmp/invite.json

# Обратное направление: телефон показал свой код, ноутбук его потребляет
curl -s -X POST $MAC/debug/identities/$MINE/establish \
  -H 'content-type: application/json' --data-binary @tmp/peer-invite.json -i

# Connections этой identity — здесь появится PdnId телефона
curl -s $MAC/debug/identities/$MINE/connections | jq
export PEER=<id из списка>
```

### Entry

Payload'ы ходят сырым телом: что записали байтами, то и прочли.

```sh
# Записать
curl -s -X PUT $MAC/debug/data/$MINE/contact/email --data-binary 'anton@example.com' -i

# Прочитать значение
curl -s $MAC/debug/data/$MINE/contact/email; echo

# Listing: path и длина payload'а, без самих байт
curl -s $MAC/debug/data/$MINE | jq
```

### Grant

Публикация заменяет grant целиком, а не дополняет его: перечисляйте все claim'ы, которые должны остаться.

```sh
# Выдать телефону два claim'а, второй с правом записи
curl -s -X POST $MAC/debug/identities/$MINE/grants/$PEER \
  -H 'content-type: application/json' -d '{
    "issuer": "'$MINE'",
    "claims": [
      {"path": "contact/email", "write": false},
      {"path": "notes/scratch", "write": true}
    ]
  }' -i

# Что выдал я (чтение из node'ы, а не из памяти скрипта)
curl -s $MAC/debug/identities/$MINE/own-grants/$PEER | jq

# Что выдали мне — по одной capability на каждого issuer'а, от имени которого peer делится
curl -s $MAC/debug/identities/$MINE/grants/$PEER | jq

# Снять grant над данными конкретного issuer'а
curl -s -X DELETE $MAC/debug/identities/$MINE/grants/$PEER/$MINE -i
```

Прочитать то, чем поделился телефон, — тем же data-маршрутом, но issuer'ом стоит identity телефона:

```sh
curl -s $MAC/debug/data/$PEER_ISSUER/contact/email; echo
```

### Linking

```sh
# Отчеканить linking payload на identity ноутбука
curl -s -X POST "$MAC/debug/identities/$MINE/linking-invite?lifetime_secs=120" > tmp/linking.json
pdnqr tmp/linking.png < tmp/linking.json

# Обратное: телефон показал свой linking-код, ноутбук присоединяется
curl -s -X POST "$MAC/debug/link?timeout_secs=30" \
  -H 'content-type: application/json' --data-binary @tmp/phone-linking.json -i
```

Адресата у `link` нет: payload сам называет identity, к которой присоединяются.

## 6. Прочитать QR с экрана телефона

У ноутбука нет камеры, направленной на телефон, поэтому код снимается скриншотом и разбирается Vision'ом. Скрипт создаётся один раз:

```sh
cat > $PDN/tmp/qrdecode.swift <<'SWIFT'
import Foundation
import Vision

let path = CommandLine.arguments[1]
guard let image = NSData(contentsOfFile: path) as Data? else { exit(2) }
let request = VNDetectBarcodesRequest()
request.symbologies = [.qr]
let handler = VNImageRequestHandler(data: image, options: [:])
try handler.perform([request])
for observation in request.results ?? [] {
    if let payload = observation.payloadStringValue { print(payload) }
}
SWIFT
```

Дальше: на телефоне снимок экрана с кодом, AirDrop на ноутбук, и

```sh
swift $PDN/tmp/qrdecode.swift ~/Downloads/IMG_XXXX.PNG 2>/dev/null | pdnraw > tmp/peer-invite.json
cat tmp/peer-invite.json | jq
```

Полученный файл — вход для `establish` или для `link` из раздела 5.

## 7. Проверка перед демонстрацией

```sh
# Один Wi-Fi и одна подсеть; адрес ноутбука, который увидит телефон
ipconfig getifaddr en0

# Node ноутбука жива и знает свои identity
curl -s $MAC/debug/status | jq
```

Руками на телефоне, потому что командой это не проверяется:

- Settings → PDN → **Local Network** включён. Выключенный доходит до экрана как `REFUSED · COUNTERPARTY-UNREACHABLE`, а не как запрет.
- Settings → PDN → **Camera** включён, иначе экран чтения кода покажет карточку об отказе.
- Приложение открыто и на экране **Identities** сказано «The node is up».

## 8. Проверки репозитория

Понадобятся, если по ходу демонстрации что-то правится.

```sh
just check                      # clippy и rustfmt по воркспейсу
just test                       # nextest, контейнерные тесты пропускаются
just test -p pdn-mobile         # только фасад
(cd pdn-app && npm run typecheck && npm run lint && npm run check)
```
