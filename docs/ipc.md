# Controlling Hitori from scripts

A running Hitori listens on a local channel. The channel lets shells and bars (noctalia, DMS, waybar, scripts)
read the connection state and drive the app. Only one instance runs per user: launching `hitori` again shows
the window of the running instance and exits.

## CLI

```sh
hitori status            # one JSON object
hitori connect           # connect to the selected server
hitori disconnect
hitori servers           # the server list with the last measured ping
hitori select <id>       # switch to a server (connects there, like picking it in the app)
hitori watch             # one JSON line now, then one for every change, until the app quits
```

Every command prints JSON lines on stdout. The exit code is 0 when the answer has `"ok": true`, 1 when it has
`"ok": false`, and 2 when Hitori is not running. `watch` exits with 1 when the app quits.

## Transport

| OS            | Channel                                                                                      |
|---------------|----------------------------------------------------------------------------------------------|
| Linux         | Unix socket `$XDG_RUNTIME_DIR/hitori/ipc.sock` (without `XDG_RUNTIME_DIR`: `~/.cache/hitori/run/ipc.sock`) |
| macOS         | Unix socket `$XDG_RUNTIME_DIR/hitori/ipc.sock`, usually `~/Library/Caches/hitori/run/ipc.sock` |
| Windows       | Named pipe `\\.\pipe\hitori-<USERNAME>` (local clients only)                                 |

The socket is mode 0600 in a 0700 directory, so only your user can connect. The Windows pipe has the default
ACL: other accounts can open it only for reading, so they cannot send commands.

## Protocol

Send one JSON object per line. Each request gets one JSON line back. You can send many requests on one
connection.

| Request                              | Answer                                                     |
|--------------------------------------|------------------------------------------------------------|
| `{"cmd":"status"}`                   | `ok` + the status object                                   |
| `{"cmd":"connect"}`                  | `ok` + status (`"ok":false` if it cannot start, see `error`) |
| `{"cmd":"disconnect"}`               | `ok` + status                                              |
| `{"cmd":"servers"}`                  | `{"ok":true,"servers":[…]}`                                |
| `{"cmd":"select","id":"<server id>"}`| `ok` + status; `"ok":false` for an unknown id              |
| `{"cmd":"show"}`                     | `{"ok":true}`: the main window opens or comes to the front |
| `{"cmd":"subscribe"}`                | a stream of status events, see below                       |

Errors look like `{"ok":false,"error":"unknown command: fly"}`.

Status (`state` is `connected`, `connecting` or `disconnected`; `traffic` is `null` when not connected;
`connected_since` is a Unix timestamp):

```json
{"ok":true,"state":"connected","signed_in":true,
 "server":{"id":"de-1","name":"Germany · Frankfurt","country":"Germany","code":"DE","city":"Frankfurt"},
 "entry":null,"connected_since":1762870000,
 "traffic":{"rx_bytes":18432000,"tx_bytes":1048576,"down_mbps":3.12,"up_mbps":0.25,"latency_ms":41},
 "error":null}
```

`entry` is the first hop of a chained route, in the same form as `server`. The byte counters count from the
moment the tunnel (re)started.

Server list entries add `ping_ms` (`null` when not measured or the server did not answer) and `selected`:

```json
{"id":"de-1","name":"Germany · Frankfurt","country":"Germany","code":"DE","city":"Frankfurt","ping_ms":41,"selected":true}
```

After `subscribe`, the connection only sends events. The first event is the current status. Then Hitori sends
one event every time the status changes: the state, the server, an error, and, while connected, the traffic
numbers about once a second. An event is the status object with `"event":"status"` instead of `ok`.

## Examples

Shell, without the CLI:

```sh
echo '{"cmd":"status"}' | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/hitori/ipc.sock"
hitori status | jq -r .state
hitori servers | jq -r '.servers[] | "\(.id)\t\(.name)\t\(.ping_ms // "-")"'
```

A bar label that updates on every change (waybar `custom` module with `"exec": "hitori-bar"`, or any script):

```sh
#!/bin/sh
hitori watch | jq --unbuffered -c '{text: (if .state == "connected" then .server.country else .state end), class: .state}'
```

noctalia and DMS are Quickshell shells, so a widget can read the stream with a `Process`:

```qml
import Quickshell.Io

Process {
    id: hitori
    property var status: ({})
    running: true
    command: ["hitori", "watch"]
    stdout: SplitParser { onRead: line => hitori.status = JSON.parse(line) }
    // The app quit or was not running: try again later.
    onExited: restart.start()
}

Timer { id: restart; interval: 5000; onTriggered: hitori.running = true }
```

A toggle button calls `hitori connect` or `hitori disconnect` depending on `status.state`.
