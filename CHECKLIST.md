# The live walk-through

Everything in this repository was built without a reachable cluster: the
tests drive a fake `KubeSource`, and `scripts/walk.py` drives the release
binary under a pty against `scripts/fake-kubectl`. Until this list is ticked
on a machine that can reach the clusters, treat the first live run as the
real acceptance test.

Tick these in order.

1. `az login`; `az account show` names the tenant the clusters are in.
2. `aks-tui setup`: every cluster is listed, credentials written, and the
   `kubelogin` line says the kubeconfig was converted. If `kubelogin` is not
   on `PATH`, install it (https://azure.github.io/kubelogin/) and run
   `kubelogin convert-kubeconfig -l azurecli` yourself — without it, a cluster
   with Entra ID sign-in asks for a device code on every read, which the
   twenty-second call cap will kill and report.
3. Trim the printed `[[clusters]]` blocks to the namespaces you want tabs
   for and put them in `~/.config/aks-tui/config.toml` (or run `setup
   --write` and edit).
4. `aks-tui doctor`: every scope `ok`, each under a second. A `forbidden`
   here is RBAC, not the tool; a `FAIL` names what to fix.
5. `aks-tui --no-cache`: the first frame inside a second, each tab's badge
   filling in over the next few seconds. Compare one tab's rows with
   `kubectl get pods -n NS` by eye: the same names, the same statuses, the
   same restart counts.
6. Quit and start again without `--no-cache`: the same rows are on the first
   frame, and the status bar says how old they are.
7. `Enter` on a chatty pod: the log follows; `k` a few times leaves follow,
   `End` resumes; `C` on a pod with a sidecar moves the stream; `P` on a
   crash-looping pod shows the run before the restart; `/` in the pane
   filters. `Esc` three times and `ps aux | grep 'kubectl logs'` shows no
   child left behind.
8. `d` and `v` on a pod, and on an event, and on a configmap: the text lands
   in the pane once and is there again when the cursor comes back.
9. `b` on a pod: a shell in it, `exit`, and the TUI repaints whole. On a
   distroless image the status bar says the exec failed instead.
10. On qa: `x` on a deployment's pod, confirm, and a new pod appears within
    the five-second read while the old one goes `Terminating`. `X` on one:
    the deployment rolls. `=` from 3 to 4 and back: the details say `4/4
    ready` once it settles.
11. `e` on a crash-looping pod: its `BackOff` events, newest first; `Enter`
    on one is back on the pod.
12. `m`: a configmap's keys; `Enter` on a multi-line key shows every line;
    `y` and paste it somewhere: byte-identical.
13. `s`: a secret's keys and sizes; `v` reveals one for sixty seconds and it
    is gone at zero; `y` on another key and paste it: byte-identical, and it
    was never on screen. Then
    `grep -rc 'the value' ~/.local/share/aks-tui/` is 0 for every file.
14. On prod, `s` where the role does not allow it: the status line says
    `prod/prod secrets: … Forbidden` and the pods tab is untouched.
15. Over SSH, from a terminal that speaks OSC 52 (kitty, WezTerm, Alacritty,
    foot, iTerm2, Windows Terminal, VS Code's): `y` still lands the text on
    the **local** clipboard. Inside tmux with `set -s set-clipboard on`: the
    same.
16. Wrong on purpose: a context in `config.toml` that does not exist reads
    `context "x" does not exist` on its tab and nowhere else; unplugging the
    network for a minute backs the reads off (the `?` help lists the
    problem) and they come back on their own.

## Things only a live run settles

- Whether the work kubeconfig already carries `kubelogin` in `azurecli` mode
  or needs the conversion in step 2.
- Whether five seconds is the right read cadence against the real API
  server, or `refresh = 3` feels better; and whether a `--watch` stream is
  worth adding over polling.
- Whether the twenty-second call cap ever fires on a healthy cluster (it
  should not; the request timeout is ten).
- How `kubectl describe` and `get -o yaml` behave on a pod with a very large
  spec: both are read once and held, so a slow one costs once per run.
