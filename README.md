# aks-tui

A fast terminal browser for AKS. One tab per namespace across your clusters;
pod health at a glance; logs, describe, YAML, events, configmaps and secrets
one key away; restart, scale and a shell into a pod without leaving the
screen. Linux, `kubectl` and `az` underneath.

It is the sibling of [ticket-tui](https://github.com/jacobragsdale/ticket-tui)
and [az-tui](https://github.com/jacobragsdale/az-tui): the same stack (Rust,
ratatui, crossterm), the same layout, the same keys.

**Status:** under construction. The scaffold is in place — tabs from
`config.toml`, search, sort, session, theme. The pods list lands next.

## Run it

```console
az login
az aks get-credentials --resource-group RG --name CLUSTER   # once per cluster
cargo install --git https://github.com/jacobragsdale/aks-tui
aks-tui
```

Or from a checkout: `cargo run --release`.

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

# refresh = 5                   # seconds between reads of the open tab
```

Every namespace is a tab, in the file's order: `1 qa/dev · 2 qa/qa ·
3 qa/uat · 4 prod`. A flag beats an `AKS_TUI_*` variable, which beats the
file.

## Keys

| Key | Does |
|---|---|
| `1`–`9`, `[` `]`, `←` `→`, click | a tab by number; the previous, the next |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move the cursor in the focused pane |
| `Tab` | focus the table or the details pane |
| `/` | search; `Esc` or `Enter` leaves the box and keeps the filter; `Esc` again clears it; `Ctrl-U` clears the box |
| `S`, header click | sort |
| `r` | read this tab again now |
| `?` | help |
| `q`, `Ctrl-C` | quit |

## Where things live

| | Path |
|---|---|
| Configuration | `$XDG_CONFIG_HOME/aks-tui/config.toml`, else `~/.config/aks-tui/config.toml` |
| Cache | `$XDG_DATA_HOME/aks-tui/cache.json`, else `~/.local/share/aks-tui/cache.json` |
| Session | the same directory, `session.json` |

## License

MIT, see [LICENSE](LICENSE).
