# aks-tui

A fast terminal browser for AKS. One tab per namespace across your clusters;
pod health at a glance; logs, describe, YAML, events, configmaps and secrets
one key away; restart, scale and a shell into a pod without leaving the
screen. Linux, `kubectl` and `az` underneath.

It is the sibling of [ticket-tui](https://github.com/jacobragsdale/ticket-tui)
and [az-tui](https://github.com/jacobragsdale/az-tui): the same stack (Rust,
ratatui, crossterm), the same layout, the same keys.

```
 1 qa/dev ✗ 1  2 qa/qa  3 qa/uat ✗ 1  4 prod                                                                  ?
/ Type / to search pods, or status:crash owner:orders-api app: node:
╭ Pods ───────────────────────────────────────────────────────────╮╭ Details ──────────────────────────────────────────╮
│  Name ↑                        Ready Status               ↻ Age ││✗ orders-worker-5c4d3e-q8zt  CrashLoopBackOff      │
│─────────────────────────────────────────────────────────────────││qa/dev · Deployment/orders-worker · 2d             │
│  billing-api-1a2b3c-qq111        2/2 ● Running            0  2d ││                                                   │
│  orders-api-7d9f5b-abc12         1/1 ● Running            2  2d ││Ready         0/1                                  │
│  orders-api-7d9f5b-k9x2p         1/1 ● Running            0  2d ││Restarts      17                                   │
│› orders-worker-5c4d3e-q8zt       0/1 ✗ CrashLoopBackOff  17  2d ││Node          aks-np1-vmss000000                   │
│  redis-0                         1/1 ● Running            0  2d ││IP            10.244.0.7                           │
│                                                                 ││Created       2026-09-10 · 2d                      │
│                                                                 ││Labels        app=orders-worker                    │
│                                                                 ││                                                   │
│                                                                 ││── Containers ─────────────────────────────────────│
│                                                                 ││✗ api  CrashLoopBackOff  ↻17                       │
│                                                                 ││  myacr.azurecr.io/team/orders-worker:1.2.3        │
│                                                                 ││  last exit: Error (1)                             │
╰ 5 · Name ↑ ─────────────────────────────────────────────────────╯╰───────────────────────────────────────────────────╯
↑↓/jk move  [ ] tabs  / search  S sort  r refresh  ? help                                        ● 5 pods · just now
```

**Status:** the pods slice is in. Tabs from `config.toml`, the pods table
with health colours, the details pane, literal search, a tab badge counting
the pods in trouble, a five-second read of the open tab and a thirty-second
one of the others, a cache the next start paints from. Logs, describe, YAML,
bash, restart, scale, events, configmaps and secrets are the next slices.

## Run it

```console
az login
az aks get-credentials --resource-group RG --name CLUSTER   # once per cluster
cargo install --git https://github.com/jacobragsdale/aks-tui
aks-tui
```

Or from a checkout: `cargo run --release`.

An AKS cluster with Entra ID sign-in wants [`kubelogin`](https://azure.github.io/kubelogin/)
on `PATH`, and its kubeconfig converted once so it borrows the `az login`
rather than asking for a device code on every read:

```console
kubelogin convert-kubeconfig -l azurecli
```

## Configuration

Copy [`config.example.toml`](config.example.toml) to
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

Every namespace is a tab, in the file's order: `1 qa/dev · 2 qa/qa ·
3 qa/uat · 4 prod`. A cluster with no `namespaces` is one tab over all of
them, with a Namespace column. A flag beats an `AKS_TUI_*` variable, which
beats the file.

## Keys

| Key | Does |
|---|---|
| `1`–`9`, `[` `]`, `←` `→`, click | a tab by number; the previous, the next |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move the cursor in the focused pane |
| `Tab` | focus the table or the details pane |
| `/` | search; `Esc` or `Enter` leaves the box and keeps the filter; `Esc` again clears it; `Ctrl-U` clears the box |
| `S`, header click | sort; the same header again turns it round, a third time is the default |
| `r` | read this tab again now |
| `?` | help |
| `q`, `Ctrl-C` | quit |

The mouse: click a row, a tab, a column header or the search row; the wheel
scrolls the pane under it.

## Searching

Every whitespace-separated word must appear somewhere in the row as a
substring, case-insensitively — the name, the namespace, the status, the
owner, the node, the images. Scattered letters match nothing. A word of the
form `key:value` is a filter when the key is one of `name:` `ns:` `status:`
`owner:` `app:` `node:`, and an ordinary word otherwise, so
`orders-api:1.2.3` finds every pod running that image. Filters and words are
all ANDed.

## How it reads

The tab on screen is read every `refresh` seconds (five by default) and the
moment it is switched to; the other tabs every thirty seconds, for their
badges. A scope that fails backs off, doubling to two minutes, and its rows
stand from the last read that worked, with the message in the status bar
and under `?`. Every `kubectl` call carries `--request-timeout=10s` and is
killed after twenty seconds regardless, which is what a credential plugin
waiting on a device-code login looks like.

The first frame is painted from the last run's pod lists, kept in
`~/.local/share/aks-tui/cache.json` (`0600`), before `kubectl` has answered.
`--no-cache` neither reads nor writes it; `--cache PATH` moves it.

## Where things live

| | Path |
|---|---|
| Configuration | `$XDG_CONFIG_HOME/aks-tui/config.toml`, else `~/.config/aks-tui/config.toml` |
| Cache | `$XDG_DATA_HOME/aks-tui/cache.json`, else `~/.local/share/aks-tui/cache.json` |
| Session | the same directory, `session.json`: the tab, each tab's sort and columns |

## Without a cluster

`scripts/fake-kubectl` answers `kubectl` from fixtures — two clusters, four
namespaces, a pod in a crash loop, one that will not pull, one terminating —
and `scripts/walk.py` drives the release binary under a pty against it and
asserts what it painted:

```console
cargo build --release
scripts/walk.py --show          # needs uv; or: pip install pyte && python3 scripts/walk.py
```

## License

MIT, see [LICENSE](LICENSE).
