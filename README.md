# aks-tui

A fast terminal browser for AKS. One tab per namespace across your clusters;
pod health at a glance; logs, describe, YAML, events, configmaps and secrets
one key away; restart, scale and a shell into a pod without leaving the
screen. Linux, `kubectl` and `az` underneath, everything clickable.

It is the sibling of [ticket-tui](https://github.com/jacobragsdale/ticket-tui)
and [az-tui](https://github.com/jacobragsdale/az-tui): the same stack (Rust,
ratatui, crossterm), the same layout, the same keys.

```
 1 qa/dev ✗ 1  2 qa/qa  3 qa/uat ✗ 1  4 prod                                                        Pods ▾  ?
/ Type / to search pods, or status:crash owner:orders-api app: node:
╭ Pods ───────────────────────────────────────────────────────────╮╭ Details ──────────────────────────────────────────╮
│  Name ↑                        Ready Status               ↻ Age ││[Logs] [Bash] [Restart] [Scale] [Describe] [YAML]  │
│─────────────────────────────────────────────────────────────────││✗ orders-worker-5c4d3e-q8zt  CrashLoopBackOff      │
│  billing-api-1a2b3c-qq111        2/2 ● Running            0  2d ││qa/dev · Deployment/orders-worker · 3/3 ready · 2d │
│  orders-api-7d9f5b-abc12         1/1 ● Running            2  2d ││                                                   │
│  orders-api-7d9f5b-k9x2p         1/1 ● Running            0  2d ││Ready         0/1                                  │
│› orders-worker-5c4d3e-q8zt       0/1 ✗ CrashLoopBackOff  17  2d ││Restarts      17                                   │
│  redis-0                         1/1 ● Running            0  2d ││Node          aks-np1-vmss000000                   │
│                                                                 │╰───────────────────────────────────────────────────╯
│                                                                 │╭ Log · following · orders-worker-5c4d3e-q8zt · api ╮
│                                                                 ││ 12:04:01 INFO  handled GET /orders                │
│                                                                 ││ 12:04:02 ERROR upstream timeout                   │
│                                                                 ││ 12:04:02 WARN  retrying in 5s                     │
╰ 5 · Name ↑ ─────────────────────────────────────────────────────╯╰───────────────────────────────────────────────────╯
j/k scroll  End follow  / filter  z zoom  P previous  C container  Tab table  Esc close      ● 5 pods · just now
```

## What it does

- **Tabs are namespaces.** `1 qa/dev · 2 qa/qa · 3 qa/uat · 4 prod`, in
  `config.toml`'s order; a number, `[` `]`, the arrows or a click switches.
  Each tab keeps its own cursor, search and kind.
- **Four kinds per tab** — Pods, Events, ConfigMaps, Secrets — on `p e m s`
  or the pill at the right of the tab bar. Each kind keeps its own cursor,
  search, sort and columns.
- **Pod health at a glance.** A pod in trouble reads red across its whole
  row and counts on its tab's `✗ N` badge; a finished one reads muted; the
  status cell carries kubectl's own word and a glyph.
- **Logs** stream into a pane under the details and follow the pod under
  the cursor; **describe** and **YAML** land in the same pane, read once per
  object. `/` inside the pane filters its lines.
- **Bash into a pod**, **restart** it (delete; its owner replaces it),
  **rollout-restart** or **scale** its owner — each one kubectl call, each
  asked about first, each reading the namespace again the moment it went.
- **Events** newest first with `e` on a pod narrowing to it; **configmaps**
  with their keys walked in the details pane and a key's value in the text
  pane; **secrets** read one key at a time, shown for sixty seconds or
  copied unseen, never cached, never logged.
- **Fast.** The first frame paints from a cache of the last pod lists; the
  open tab is read every five seconds and the others every thirty; every
  read is one thread away from the UI; search is literal and in memory.
- **Never writes** a configmap or a secret, never `apply`s, never deletes
  anything but a pod.

## Run it

```console
az login
cargo install --git https://github.com/jacobragsdale/aks-tui
aks-tui setup          # az aks list, get-credentials, kubelogin, a [[clusters]] block per cluster
aks-tui doctor         # kubectl, kubelogin, az, and every scope answering
aks-tui
```

Or from a checkout: `cargo run --release`.

An AKS cluster with Entra ID sign-in wants [`kubelogin`](https://azure.github.io/kubelogin/)
on `PATH`, and its kubeconfig converted once so it borrows the `az login`
rather than asking for a device code on every read; `setup` does the
conversion when `kubelogin` is there:

```console
kubelogin convert-kubeconfig -l azurecli
```

`az` is used by `setup` and `doctor` only. Everything the TUI does goes
through `kubectl`.

## Configuration

`setup` prints a block per cluster to trim; or copy
[`config.example.toml`](config.example.toml) to
`~/.config/aks-tui/config.toml`:

```toml
[[clusters]]
name = "qa"
context = "aks-qa"              # kubeconfig context; left out: the name
namespaces = ["dev", "qa", "uat"]

[[clusters]]
name = "prod"
namespaces = ["prod"]

# refresh = 5                   # seconds between reads of the open tab; 0 = only `r`
```

Every namespace is a tab. A cluster with no `namespaces` is one tab over
all of them, with a Namespace column. A flag beats an `AKS_TUI_*` variable,
which beats the file. The `[theme]` table is the one ticket-tui and az-tui
read, so the `theme` tool writes one file for all three.

## Keys

| Key | Does |
|---|---|
| `1`–`9`, `[` `]`, `←` `→`, click | a tab by number; the previous, the next |
| `p` `e` `m` `s`, the pill | Pods, Events, ConfigMaps, Secrets; `e` on a pod is that pod's events |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move the cursor in the focused pane |
| `Tab` | focus the table or the pane under the details |
| `/` | search; in the text pane, filter its lines; `Esc` keeps it, `Esc` again clears it; `Ctrl-U` empties the box |
| `Enter`, `l` | Pods: the log, following. Events: the pod it is about. ConfigMaps, Secrets: the key's value |
| `d`, `v` | describe / YAML of what is under the cursor; `v` on a configmap or secret is the key's value |
| `P`, `C`, `End`, `z` | the log before the last restart; the next container; follow again; the pane alone and back |
| `b` | a shell in the pod: bash, or sh when there is none |
| `x`, `X` | restart the pod (asks first; refused with no owner); rollout-restart its owner |
| `=` | scale the pod's deployment, statefulset or replicaset |
| `y`, `Y` | copy the name — on a configmap or secret, the key's value, unseen; copy the kubectl line for what the pane shows |
| `S`, header click | sort; the same header again turns it round, a third time is the default |
| `r` | read this tab again now |
| `?` | help: the keys and every kind's search grammar |
| `q`, `Ctrl-C` | quit |

The mouse: rows, tabs, the kind pill and its menu, column headers, the
search row, the toolbar buttons in the details pane, the keys of a
configmap or secret, a modal's buttons; the wheel scrolls the pane under it.
Anything a key does, something on screen does too.

## Searching

Every whitespace-separated word must appear somewhere in the row as a
substring, case-insensitively. Scattered letters match nothing. A word of
the form `key:value` is a filter when the kind knows the key, and an
ordinary word otherwise, so `orders-api:1.2.3` finds every pod running that
image. Filters and words are all ANDed.

| Kind | Filters |
|---|---|
| Pods | `name:` `ns:` `status:` `owner:` `app:` `node:` |
| Events | `type:` `reason:` `object:` `kind:` `message:` `ns:` |
| ConfigMaps | `name:` `ns:` `key:` |
| Secrets | `name:` `ns:` `type:` `key:` |

## How it reads

The tab on screen is read every `refresh` seconds (five by default) and the
moment it is switched to; the other tabs every thirty, for their badges. A
kind other than pods is read for the open tab only, while it shows. A scope
that fails backs off, doubling to two minutes, and its rows stand from the
last read that worked, with the message in the status bar and under `?`.
Every `kubectl` call carries `--request-timeout=10s` and is killed after
twenty seconds regardless, which is what a credential plugin waiting on a
device-code login looks like.

Describe, YAML and an owner's replica count are read once per object per
run, the owner once the cursor rests on a pod for 150 ms. One
`kubectl logs -f` runs at a time and is killed when the pane leaves it and
when the run ends.

## Where things live

| | Path |
|---|---|
| Configuration | `$XDG_CONFIG_HOME/aks-tui/config.toml`, else `~/.config/aks-tui/config.toml` |
| Cache | `$XDG_DATA_HOME/aks-tui/cache.json`, else `~/.local/share/aks-tui/cache.json` |
| Session | the same directory, `session.json`: the tab, each tab's kind, the pods table's sort and columns |

**The cache** holds the last pod list per scope and nothing else: no
events, no configmap data, no secret's shape. It is written `0600`.
`--no-cache` neither reads nor writes it; `--cache PATH` moves it.

**A secret's value** is read only when `v` or `y` asks, one key at a time,
and lives in a type whose `Debug` and `Display` print `[redacted]` and which
cannot be serialised. It is dropped when the cursor moves, on `r`, on a kind
or tab switch, on quit, and sixty seconds after it arrived. `y` copies it
without ever drawing it.

## Copying over SSH

`y` writes an **OSC 52** escape to the terminal as well as trying `wl-copy`,
`xclip`, `xsel` and `clip.exe`. The escape is what makes copying work over
SSH and inside a VDI. Under tmux add `set -s set-clipboard on` (tmux 3.3 or
newer).

## Without a cluster

`scripts/fake-kubectl` answers `kubectl` from fixtures — two clusters, four
namespaces, a pod in a crash loop, one that will not pull, events,
configmaps, secrets, a namespace that refuses them — and `scripts/walk.py`
drives the release binary under a pty against it and asserts what it
painted, every key and every kind:

```console
cargo build --release
scripts/walk.py --show          # needs uv; or: pip install pyte && python3 scripts/walk.py
```

[`CHECKLIST.md`](CHECKLIST.md) is the walk-through for the first run against
real clusters, which this repository has not had yet.

## License

MIT, see [LICENSE](LICENSE).
