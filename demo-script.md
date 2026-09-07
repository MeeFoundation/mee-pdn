# Сценарий презентации: команды по порядку

Два узла: телефон с приложением и node ноутбука. Справочник по всем командам — `demo-commands.md`; здесь только та последовательность, которую ведут вживую.

Блоки команд запускает ведущий: они и есть вторая сторона демонстрации — телефон показывает интерфейс, ноутбук изображает второе устройство. Раздел 0 и «Предполётная проверка» выполняются до прихода зрителей; блоки актов 1–9 — при них, по ходу рассказа.

Все команды разложены по скриптам в `demo/` — по одному на акт, `demo/00-start.sh`, `demo/01-nodes.sh` и так далее; их список и что делает каждый — в `demo/README.md`. Ниже те же команды россыпью, если по ходу понадобится вмешаться руками.

Каждый блок можно скопировать целиком и вставить в терминал. Ожидания встроены: где данные едут по сети, команда ждёт их сама, а не показывает зрителям пустоту.

Значения текущего прогона подставлены. После перезапуска node'ы на новом каталоге замените `MINE` тем, что вернёт `POST /debug/identities`.

## 0. Преамбула — один раз перед началом

```sh
cd ~/vsprojects/mee/mee-pdn
export PDN=$PWD
export MAC=http://127.0.0.1:3011
export MINE=dc9521aa34d45d9c7be6b702e6eb7c52b0afd7b6ce2636901ec64722540a1cff
export DEV=5A613C8A-3804-5F62-A945-C7D8D934D088
```

Три функции на весь прогон. Ceremony-код, который читает телефон, — это не JSON, а base64url без padding'а поверх него: так его чеканит и разбирает фасад `pdn-mobile`, тогда как debug-поверхность HTTP отдаёт и принимает голый JSON. Третья функция ждёт того, что едет по сети, и печатает, как только дождалась.

```sh
# JSON на входе -> QR на экране, в той форме, в какой его ждёт телефон
pdnqr()  { base64 | tr -d '\n' | tr '+/' '-_' | tr -d '=' | qrencode -l L -s 6 -o "$1" && open "$1"; }
# код, снятый с экрана телефона -> JSON, который принимает /debug
pdnraw() { python3 -c "import base64,sys;d=sys.stdin.read().strip();sys.stdout.write(base64.urlsafe_b64decode(d+'='*(-len(d)%4)).decode())"; }
# ждать до 48 секунд, пока команда напечатает что-нибудь
pdnwait() { for i in $(seq 1 24); do out=$(eval "$1") && [ -n "$out" ] && { echo "$out"; return 0; }; sleep 2; done; echo "не дождались за 48 секунд"; return 1; }
```

Поднять node ноутбука. Каталог `tmp/pdn-mac-node2` переживает перезапуск: identity и connection'ы возвращаются те же. Для прогона с чистого листа сначала выполните раздел «Полный сброс между прогонами».

```sh
# Первый блок обязателен: без $PDN и $MAC следующая строка запускает не тот путь
[ -x "$PDN/target/debug/pdn-node-http" ] || cargo build -p pdn-node-http
# Прежняя node должна отпустить lock каталога, и на это уходит больше секунды
pkill -f 'target/debug/pdn-node-http'
for i in $(seq 1 15); do pgrep -f 'target/debug/pdn-node-http' >/dev/null || break; sleep 1; done
PDN_DATA_DIR=$PDN/tmp/pdn-mac-node2 PDN_DEBUG=1 \
  nohup $PDN/target/debug/pdn-node-http > $PDN/tmp/pdn-mac-node2.log 2>&1 & disown
for i in $(seq 1 20); do curl -sf $MAC/ready >/dev/null && break; sleep 1; done
curl -sf $MAC/debug/status || { echo "node не поднялась, вот её лог:"; tail -20 $PDN/tmp/pdn-mac-node2.log; }
```

Если `debug/status` не назвал ни одной identity — каталог чистый, и её надо завести:

```sh
export MINE=$(curl -s -X POST $MAC/debug/identities | jq -r .identity)
echo "identity ноутбука: $MINE"
```

Вторая вкладка — оттуда видно церемонию и sync:

```sh
tail -f $PDN/tmp/pdn-mac-node2.log
```

Третья, если хочется показывать, что говорит телефон. Команда сама запускает приложение и держит его stderr; отдельного «подключиться к работающему» у `devicectl` нет:

```sh
xcrun devicectl device process launch --console --device $DEV org.mee.pdn.app
```

---

## Акт 1. Два независимых узла

**Телефон.** Открыть приложение → **Bring the node up** → **Create an identity**. Показать зрителям node id на карточке.

**Ноутбук.** Свой node id и свои identity:

```sh
curl -s $MAC/debug/status
curl -s $MAC/debug/identities | jq
```

Мысль вслух: узлов два, общего сервера нет, ключи у каждого свои.

---

## Акт 2. Свои данные — у себя

**Телефон.** **My entries** → путь `contact/email`, значение `anton@example.com` → **Write**. Затем **Read it** под строкой.

**Ноутбук.** То же самое своей identity:

```sh
curl -s -X PUT $MAC/debug/data/$MINE/contact/email --data-binary 'laptop@example.com'
curl -s $MAC/debug/data/$MINE | jq
```

Мысль вслух: пока связи нет, ни один узел не знает о данных другого — он не знает даже, что такой issuer существует.

---

## Акт 3. Церемония

Сначала дождаться, пока endpoint ноутбука осядет и получит home relay: первые секунды после подъёма node'ы код несёт только локальные адреса, и связность за пределами одной сети теряется.

```sh
pdnwait 'curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=10" | jq -re ".inviter_addr.addrs[]|select(.Relay)|.Relay"'
```

**Чеканим invite и показываем его QR'ом:**

```sh
curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=180" > tmp/invite.json
jq '.inviter_addr.addrs' tmp/invite.json
pdnqr tmp/invite.png < tmp/invite.json
```

**Телефон.** **Connections** → **Read a code** → акт **Accepting an invitation to connect** → навести на экран ноутбука. Пока идёт церемония, на экране «Running the ceremony. It ends within 30 seconds either way».

**Обе стороны видят друг друга.** Забрать identity телефона в переменную — она нужна во всех следующих актах:

```sh
export PEER=$(pdnwait 'curl -s $MAC/debug/identities/$MINE/connections | jq -re ".connections[0]"')
echo "телефон: $PEER"
```

Мысль вслух: connection доказывает, что два устройства держали один секрет, — и ничего больше. Ни кто человек, ни что он сказал правду.

---

## Акт 4. Телефон делится с ноутбуком

**Телефон.** **Connections** → строка ноутбука → карточка **Share claims with this peer** → тап по пути → **Grant read-only**. Путь подсветится и появится в «What I share with this peer».

**Ноутбук ждёт grant, потом читает значение.** Между нажатием на телефоне и появлением значения на ноутбуке проходит около десяти секунд: сначала едет запись grant'а, потом отдельно — сам payload.

```sh
pdnwait 'curl -s $MAC/debug/identities/$MINE/grants/$PEER | jq -ce "select(.grants|length>0)"' | jq
pdnwait 'curl -sf $MAC/debug/data/$PEER/name'
```

Подставьте вместо `name` тот путь, который выдали с телефона. Что вообще доехало под этим issuer'ом:

```sh
curl -s $MAC/debug/data/$PEER | jq
```

Мысль вслух: grant назвал issuer'а и ровно один claim; всё остальное под этим issuer'ом сюда не реплицируется вовсе. Показать это можно так:

```sh
curl -s -w ' [HTTP %{http_code}]\n' $MAC/debug/data/$PEER/notes/private
```

Только честно назвать, что показано: ответ будет `404 no entry`, а не отказ. Пока grant'а не было, ответ был другой — `409 data namespace not bound on this node`, namespace не был привязан вообще. После grant'а namespace привязан, и негранованный путь неотличим от несуществующего. Настоящая проверка границы — третья node, которой не выдавали ничего; на телефоне она не показывается.

---

## Акт 5. Ноутбук делится с телефоном

```sh
curl -s -X POST $MAC/debug/identities/$MINE/grants/$PEER \
  -H 'content-type: application/json' -d '{
    "issuer": "'$MINE'",
    "claims": [{"path": "contact/email", "write": false}]
  }' -w 'HTTP %{http_code}\n'
curl -s $MAC/debug/identities/$MINE/own-grants/$PEER | jq
```

Ответ `HTTP 204` — публикация принята. В `own-grants` claim'ы стоят хэшами: identity claim'а выводится односторонне из issuer'а и пути, и обратно путь не восстанавливается.

**Телефон.** На экране connection'а карточка **What this peer shares with me** наполняется сама: путь, пометка read-only, значение `laptop@example.com`.

---

## Акт 6. Снять и выдать заново

**Телефон.** «What I share with this peer» → **Withdraw this grant**.

**Ноутбук.** Grant'ов больше нет, и namespace отвязан — тот самый `409`, что был до знакомства:

```sh
pdnwait 'curl -s $MAC/debug/identities/$MINE/grants/$PEER | jq -ce "select((.grants|length)==0)"'
curl -s -w ' [HTTP %{http_code}]\n' $MAC/debug/data/$PEER/name
```

**Телефон.** Снова **Grant read-only** — доступ открывается обратно:

```sh
pdnwait 'curl -sf $MAC/debug/data/$PEER/name'
```

Мысль вслух: withdraw закрывает дальнейшую доставку, но не отзывает уже доставленное. Обещание аккуратное и честное.

---

## Акт 7. Право записи

**Ноутбук выдаёт телефону два claim'а, второй с правом записи:**

```sh
curl -s -X PUT $MAC/debug/data/$MINE/notes/shared --data-binary 'первая строка с ноутбука'
curl -s -X POST $MAC/debug/identities/$MINE/grants/$PEER \
  -H 'content-type: application/json' -d '{
    "issuer": "'$MINE'",
    "claims": [
      {"path": "contact/email", "write": false},
      {"path": "notes/shared", "write": true}
    ]
  }' -w 'HTTP %{http_code}\n'
```

**Телефон.** Под writable-claim'ом появилось поле **write a new value** — вписать текст и отправить. У read-only claim'а поля нет вовсе.

**Ноутбук видит написанное телефоном в своих же данных:**

```sh
pdnwait 'curl -sf $MAC/debug/data/$MINE/notes/shared'
```

---

## Акт 8. Приложение свернули

**Телефон.** Свернуть жестом home. Не блокировать экран: блокировка может завершить процесс, и тогда измеряется другое.

**Ноутбук пишет, пока телефона «нет»:**

```sh
# время в строке: повторный прогон с тем же текстом на экране телефона неотличим от прошлого
curl -s -X PUT $MAC/debug/data/$MINE/contact/email \
  --data-binary "изменилось пока телефон спал, $(date +%H:%M:%S)"
```

**Телефон.** Вернуться в приложение. Значение приезжает само, без повторного bring-up'а.

---

## Акт 9. Всё переживает перезапуск

**Телефон.** Закрыть приложение свайпом, открыть, **Bring the node up**. Node id и identity те же, connection на месте, данные на месте.

**Ноутбук — то же самое.** Шаблон в `pkill` намеренно с путём: короткое `pdn-node-http` совпадает и с командной строкой сборки, и убивает не то.

```sh
# Первый блок обязателен: без $PDN и $MAC следующая строка запускает не тот путь
[ -x "$PDN/target/debug/pdn-node-http" ] || cargo build -p pdn-node-http
# Прежняя node должна отпустить lock каталога, и на это уходит больше секунды
pkill -f 'target/debug/pdn-node-http'
for i in $(seq 1 15); do pgrep -f 'target/debug/pdn-node-http' >/dev/null || break; sleep 1; done
PDN_DATA_DIR=$PDN/tmp/pdn-mac-node2 PDN_DEBUG=1 \
  nohup $PDN/target/debug/pdn-node-http > $PDN/tmp/pdn-mac-node2.log 2>&1 & disown
for i in $(seq 1 20); do curl -sf $MAC/ready >/dev/null && break; sleep 1; done
curl -sf $MAC/debug/status || { echo "node не поднялась, вот её лог:"; tail -20 $PDN/tmp/pdn-mac-node2.log; }
```

Мысль вслух: node возвращается на свой каталог собой. Не возвращается только незавершённое — отчеканенный и не потреблённый invite, прерванная церемония. Home relay тоже устанавливается заново, на это уходит около десяти секунд.

---

## Если что-то пошло не так

| Симптом | Причина и что делать |
| --- | --- |
| `REFUSED · MALFORMED-INPUT`, подпись «refused by this application before the node was called» | В QR попал голый JSON. Телефон ждёт base64url поверх него — рисовать код только через `pdnqr` |
| `REFUSED · COUNTERPARTY-UNREACHABLE` | У приложения выключен Local Network (Settings → PDN), либо invite отчеканен до того, как поднялся home relay. Проверить `pdnwait` на relay из акта 3 |
| Код на экране не читается | Увеличить масштаб в `pdnqr` (`-s 8`), убрать отражение, дать телефону 20–30 см |
| Церемония висит и падает по таймауту | Invite протух: `lifetime_secs` истёк. Чеканить заново |
| `pdnwait` печатает «не дождались» | Смотреть `tail -f $PDN/tmp/pdn-mac-node2.log`: там видно, идёт ли sync и с каким peer'ом |
| На экране `FAILED · INTERNAL` | Перезапустить приложение через `devicectl … --console` и смотреть его stderr |
| Экран чтения кода пуст | Камера запрещена приложению. Settings → PDN → Camera |

## Полный сброс между прогонами

```sh
pkill -f 'target/debug/pdn-node-http'
rm -rf $PDN/tmp/pdn-mac-node2 && mkdir -p $PDN/tmp/pdn-mac-node2
xcrun devicectl device uninstall app --device $DEV org.mee.pdn.app
APP=$(ls -dt ~/Library/Developer/Xcode/DerivedData/PDN-*/Build/Products/Release-iphoneos/PDN.app | head -1)
xcrun devicectl device install app --device $DEV "$APP"
```

Удаление приложения стирает каталог вместе с ключом node'ы: это потеря единственной копии, а не очистка кэша. Ровно то, что нужно для чистого прогона, и ровно то, чего нельзя делать по ошибке.

## Предполётная проверка

```sh
# Инструмент на месте
which jq qrencode

# QR-путь целиком: отчеканить, нарисовать, прочитать обратно
curl -s -X POST "$MAC/debug/identities/$MINE/invite?lifetime_secs=60" > tmp/probe.json
pdnqr tmp/probe.png < tmp/probe.json
diff tmp/probe.json <(swift $PDN/tmp/qrdecode.swift tmp/probe.png 2>/dev/null | pdnraw) \
  && echo "QR читается и разбирается в исходный invite" && rm -f tmp/probe.json tmp/probe.png

# Адреса, которые node кладёт в invite
curl -s -X POST "$MAC/debug/identities/$MINE/invite" | jq '.inviter_addr.addrs'
```

Что должно получиться: код около 490 символов, round-trip без расхождений, и четыре адреса — relay `euc1-1.relay.n0.iroh.link`, публичный адрес, найденный через NAT, и два локальных.
