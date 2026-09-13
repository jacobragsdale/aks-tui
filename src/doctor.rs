//! `aks-tui doctor` and `aks-tui setup`: the two subcommands, and the only
//! place `az` is run.
//!
//! `doctor` says whether this machine can do what the TUI will ask of it:
//! `kubectl` and `kubelogin` on `PATH`, an `az login`, and every scope in
//! `config.toml` answering `get pods`. It exits 1 when any scope does not.
//!
//! `setup` does the first-day work: `az aks list`, `az aks get-credentials`
//! for each cluster, `kubelogin convert-kubeconfig -l azurecli` so kubectl
//! borrows the `az login` rather than asking for a device code on every
//! read, and a `[[clusters]]` block per cluster with its namespaces, printed
//! to trim into `config.toml` — or written there with `--write` when the file
//! does not exist yet.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::{Config, Tab};
use crate::kube::run_capped;

/// The bound on every call `doctor` and `setup` make. `az aks list` over a
/// slow tenant is the one that comes close.
const CALL_CAP: Duration = Duration::from_secs(60);

/// Namespaces every AKS cluster has that nobody wants a tab for.
const SYSTEM_NAMESPACES: &[&str] = &[
    "default",
    "kube-system",
    "kube-public",
    "kube-node-lease",
    "gatekeeper-system",
    "calico-system",
    "tigera-operator",
    "azure-arc",
    "app-routing-system",
    "aks-command",
    "aks-istio-system",
    "aks-istio-ingress",
    "aks-istio-egress",
];

fn command(program: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(arguments);
    command
}

fn took(started: Instant) -> String {
    let millis = started.elapsed().as_millis();
    if millis < 1000 {
        format!("{millis} ms")
    } else {
        format!("{:.1} s", started.elapsed().as_secs_f64())
    }
}

fn line(out: &mut impl Write, label: &str, said: &str) -> Result<()> {
    writeln!(out, "{label:<14}{said}").context("writing the report")
}

/// The tool's own version line, or why it will not answer.
fn version(program: &str, arguments: &[&str]) -> Result<String> {
    let raw = run_capped(command(program, arguments), CALL_CAP)?;
    Ok(raw.lines().next().unwrap_or_default().trim().to_owned())
}

/// Checks the machine and the configuration; answers whether every scope
/// answered.
pub fn doctor(out: &mut impl Write, config: &Config) -> Result<bool> {
    let mut ok = true;

    match run_capped(
        command("kubectl", &["version", "--client", "-o", "json"]),
        CALL_CAP,
    ) {
        Ok(raw) => {
            let version: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            let said = version["clientVersion"]["gitVersion"]
                .as_str()
                .unwrap_or("ok")
                .to_owned();
            line(out, "kubectl", &said)?;
        }
        Err(error) => {
            ok = false;
            line(out, "kubectl", &format!("FAIL {error:#}"))?;
        }
    }
    match version("kubelogin", &["--version"]) {
        Ok(said) => line(out, "kubelogin", &said)?,
        Err(_) => line(
            out,
            "kubelogin",
            "not on PATH — a cluster with Entra ID sign-in needs it (https://azure.github.io/kubelogin/)",
        )?,
    }
    match run_capped(command("az", &["account", "show", "-o", "json"]), CALL_CAP) {
        Ok(raw) => {
            let account: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            line(
                out,
                "az login",
                &format!(
                    "{} · tenant {}",
                    account["user"]["name"].as_str().unwrap_or("?"),
                    account["tenantId"].as_str().unwrap_or("?")
                ),
            )?;
        }
        Err(error) => line(out, "az login", &format!("no — {error:#}"))?,
    }

    let tabs = config.tabs();
    if tabs.is_empty() {
        line(
            out,
            "config",
            "no [[clusters]] — run `aks-tui setup`, or copy config.example.toml",
        )?;
        return Ok(false);
    }
    writeln!(out)?;
    for tab in &tabs {
        let started = Instant::now();
        let (said, fine) = match probe(tab) {
            Ok(()) => (format!("ok ({})", took(started)), true),
            Err(error) => {
                let message = format!("{error:#}");
                let word = if message.contains("Forbidden") {
                    "forbidden"
                } else {
                    "FAIL"
                };
                (format!("{word} — {message}"), false)
            }
        };
        ok &= fine;
        line(out, &tab.scope.describe(), &said)?;
    }
    Ok(ok)
}

/// One cheap read of the scope: the first pod's name, or nothing.
fn probe(tab: &Tab) -> Result<()> {
    let mut arguments = vec![
        "--context",
        tab.scope.context.as_str(),
        "--request-timeout=10s",
        "get",
        "pods",
        "--limit=1",
        "-o",
        "name",
    ];
    match &tab.scope.namespace {
        Some(namespace) => arguments.extend(["-n", namespace]),
        None => arguments.push("--all-namespaces"),
    }
    run_capped(command("kubectl", &arguments), CALL_CAP).map(drop)
}

/// One cluster `az aks list` named.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cluster {
    pub name: String,
    pub resource_group: String,
    pub namespaces: Vec<String>,
}

/// Whether a namespace is one AKS put there rather than the team.
#[must_use]
pub fn is_system_namespace(name: &str) -> bool {
    SYSTEM_NAMESPACES.contains(&name)
}

/// The `[[clusters]]` block for one cluster, as `config.toml` takes it.
#[must_use]
pub fn cluster_block(cluster: &Cluster) -> String {
    let namespaces: Vec<String> = cluster
        .namespaces
        .iter()
        .map(|namespace| format!("{namespace:?}"))
        .collect();
    format!(
        "[[clusters]]\nname = {:?}\ncontext = {:?}\nnamespaces = [{}]\n",
        cluster.name,
        cluster.name,
        namespaces.join(", ")
    )
}

/// Fetches credentials for every AKS cluster the login can see, converts
/// the kubeconfig to borrow the `az login`, and prints a `[[clusters]]`
/// block per cluster. With `write`, the blocks go to `config_path` when no
/// file is there yet.
pub fn setup(out: &mut impl Write, write: bool, config_path: &Path) -> Result<bool> {
    let raw = run_capped(command("az", &["aks", "list", "-o", "json"]), CALL_CAP)
        .context("az aks list — is there an `az login`?")?;
    let listed: Value =
        serde_json::from_str(&raw).context("az answered with something other than JSON")?;
    let mut clusters: Vec<Cluster> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some(Cluster {
                name: item["name"].as_str()?.to_owned(),
                resource_group: item["resourceGroup"].as_str()?.to_owned(),
                namespaces: Vec::new(),
            })
        })
        .collect();
    if clusters.is_empty() {
        bail!(
            "az aks list found no clusters in the current subscription; `az account set --subscription …` and try again"
        );
    }
    for cluster in &clusters {
        let started = Instant::now();
        run_capped(
            command(
                "az",
                &[
                    "aks",
                    "get-credentials",
                    "--resource-group",
                    &cluster.resource_group,
                    "--name",
                    &cluster.name,
                    "--overwrite-existing",
                ],
            ),
            CALL_CAP,
        )
        .with_context(|| format!("az aks get-credentials for {}", cluster.name))?;
        line(
            out,
            &cluster.name,
            &format!("credentials written ({})", took(started)),
        )?;
    }
    match run_capped(
        command("kubelogin", &["convert-kubeconfig", "-l", "azurecli"]),
        CALL_CAP,
    ) {
        Ok(_) => line(out, "kubelogin", "kubeconfig converted to use the az login")?,
        Err(error) => line(
            out,
            "kubelogin",
            &format!(
                "not converted — {error:#} (a cluster with Entra ID sign-in will ask for a device code until it is)"
            ),
        )?,
    }
    for cluster in &mut clusters {
        let names = run_capped(
            command(
                "kubectl",
                &[
                    "--context",
                    &cluster.name,
                    "--request-timeout=10s",
                    "get",
                    "namespaces",
                    "-o",
                    "name",
                ],
            ),
            CALL_CAP,
        );
        match names {
            Ok(raw) => {
                cluster.namespaces = raw
                    .lines()
                    .filter_map(|held| held.trim().strip_prefix("namespace/"))
                    .filter(|name| !is_system_namespace(name))
                    .map(str::to_owned)
                    .collect();
            }
            Err(error) => line(
                out,
                &cluster.name,
                &format!("namespaces not read — {error:#}; list them by hand"),
            )?,
        }
    }
    let blocks: Vec<String> = clusters.iter().map(cluster_block).collect();
    let text = blocks.join("\n");
    writeln!(out)?;
    if write {
        if config_path.exists() {
            writeln!(
                out,
                "{} exists; not touched. The blocks it would have had:\n",
                config_path.display()
            )?;
            write!(out, "{text}")?;
            return Ok(false);
        }
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to make {}", parent.display()))?;
        }
        std::fs::write(
            config_path,
            format!("{text}\n# refresh = 5\n\n[theme]\n# preset = \"terminal\"\n"),
        )
        .with_context(|| format!("failed to write {}", config_path.display()))?;
        writeln!(
            out,
            "wrote {} — trim the namespaces to the ones you want tabs for",
            config_path.display()
        )?;
    } else {
        writeln!(
            out,
            "# Trim to the namespaces you want tabs for and put in {}\n# (or run `aks-tui setup --write` to write it):\n",
            config_path.display()
        )?;
        write!(out, "{text}")?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cluster_block_is_config_toml_the_file_reads_back() {
        let block = cluster_block(&Cluster {
            name: "aks-qa".into(),
            resource_group: "rg-qa".into(),
            namespaces: vec!["dev".into(), "qa".into(), "uat".into()],
        });
        assert_eq!(
            block,
            "[[clusters]]\nname = \"aks-qa\"\ncontext = \"aks-qa\"\nnamespaces = [\"dev\", \"qa\", \"uat\"]\n"
        );
        let config = crate::config::parse(&block).unwrap();
        assert_eq!(config.tabs().len(), 3);
        assert_eq!(config.tabs()[0].label, "aks-qa/dev");
    }

    #[test]
    fn the_namespaces_aks_puts_there_are_left_out() {
        assert!(is_system_namespace("kube-system"));
        assert!(is_system_namespace("gatekeeper-system"));
        assert!(is_system_namespace("default"));
        assert!(!is_system_namespace("dev"));
        assert!(!is_system_namespace("prod"));
    }

    #[test]
    fn a_configuration_with_no_clusters_fails_the_doctor_before_any_probe() {
        let mut out = Vec::new();
        let ok = doctor(&mut out, &Config::default()).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(!ok);
        assert!(said.contains("kubectl"), "{said}");
        assert!(said.contains("no [[clusters]]"), "{said}");
    }

    #[test]
    fn setup_without_az_says_so_rather_than_panicking() {
        // Whatever this box has, `az aks list` either answers or the error
        // names the command; both are a result, never a panic.
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let outcome = setup(&mut out, false, &dir.path().join("config.toml"));
        if let Err(error) = outcome {
            assert!(format!("{error:#}").contains("az"), "{error:#}");
        }
    }
}
