/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::batch::v1::JobSpec;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::core::v1::PodTemplateSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use mononoke_types::typed_hash::RedactionKeyListId;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::Instant;

const AWS_CLOUD: &str = "mononoke-cloud";
const AWS_REGION: &str = "us-west-2";
const AWS_NAMESPACE: &str = "mononoke-prod";
const AWS_DEPLOYMENT: &str = "mononoke-server";
const AWS_CONTAINER: &str = "server";
const AWS_LOG_TAILER_CONTAINER: &str = "tailer";
const AWS_SYNC_JOB_LABEL: &str = "redaction-sync-job";
const EKS_CONFIG_TIMEOUT: Duration = Duration::from_secs(30);
const KUBECTL_TIMEOUT: Duration = Duration::from_secs(30);
const KUBECTL_EXEC_TIMEOUT: Duration = Duration::from_secs(180);
const KUBECTL_EXEC_PROCESS_TIMEOUT: Duration = Duration::from_secs(240);
const BOOTSTRAP_STRIP_TIMEOUT: Duration = Duration::from_secs(60);
const BOOTSTRAP_UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);
const JOB_DEADLINE_SECONDS: u64 = 360;
const BOOTSTRAP_JOB_DEADLINE_SECONDS: u64 = 600;
const JOB_START_TIMEOUT: Duration = Duration::from_secs(90);
const JOB_TTL_SECONDS: u64 = 300;
const JOB_COMPLETION_FILE: &str = "/tmp/mononoke-redaction-sync-done";
const BOOTSTRAP_MONAD_PATH: &str = "/tmp/mononoke-admin-bootstrap";
const RESULT_PREFIX: &str = "MONONOKE_AWS_SYNC_RESULT=";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AwsKeyList {
    pub id: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PodList {
    items: Vec<Pod>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AwsSyncItemResult {
    AlreadyPresent { id: String },
    Inserted { id: String },
    Failed { id: String, error: String },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AwsSyncReport {
    pub items: Vec<AwsSyncItemResult>,
}

impl AwsSyncItemResult {
    pub fn already_present(id: impl Into<String>) -> Self {
        Self::AlreadyPresent { id: id.into() }
    }

    pub fn inserted(id: impl Into<String>) -> Self {
        Self::Inserted { id: id.into() }
    }

    pub fn failed(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Failed {
            id: id.into(),
            error: error.into(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::AlreadyPresent { id } | Self::Inserted { id } | Self::Failed { id, .. } => id,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error, .. } => Some(error),
            Self::AlreadyPresent { .. } | Self::Inserted { .. } => None,
        }
    }
}

impl AwsSyncReport {
    pub fn has_failures(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item, AwsSyncItemResult::Failed { .. }))
    }
}

pub fn print_sync_report(report: &AwsSyncReport) -> Result<()> {
    println!(
        "{RESULT_PREFIX}{}",
        serde_json::to_string(report).context("Failed to serialize AWS sync result")?
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub enum AwsSyncMethod {
    RunningPod,
    TemporaryJob,
}

#[derive(Clone, Copy, Debug)]
pub enum TemporaryJobMonad {
    Deployed,
    Bootstrap,
}

#[derive(Clone, Copy, Debug)]
enum ExecMode {
    RunningPod,
    TemporaryJob(TemporaryJobMonad),
}

pub enum AwsSyncTarget {
    RunningPod(String),
    TemporaryJob(TemporaryJobMonad),
}

struct BootstrapMonad {
    path: PathBuf,
}

impl Drop for BootstrapMonad {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn build_discover_cmd() -> Vec<String> {
    vec![
        "get".to_string(),
        "pods".to_string(),
        "-l".to_string(),
        format!("app.kubernetes.io/name=mononoke-server,!{AWS_SYNC_JOB_LABEL}"),
        "--field-selector=status.phase=Running".to_string(),
        "--request-timeout=30s".to_string(),
        "-o".to_string(),
        "json".to_string(),
    ]
}

fn import_key_lists_command(mode: ExecMode) -> Vec<String> {
    let command = match mode {
        ExecMode::RunningPod => {
            return vec![
                "monad".to_string(),
                "redaction".to_string(),
                "sync-key-lists-from-json".to_string(),
                "--payload-stdin".to_string(),
            ];
        }
        ExecMode::TemporaryJob(TemporaryJobMonad::Deployed) => {
            "monad redaction sync-key-lists-from-json --payload-stdin".to_string()
        }
        ExecMode::TemporaryJob(TemporaryJobMonad::Bootstrap) => format!(
            "{BOOTSTRAP_MONAD_PATH} --acl-file /cfg/acls/acls --config-tier aws --local-configerator-path /cfg --cache-mode local-only --just-knobs-config-path /cfg/justknobs/justknobs.json --log-level=OFF redaction sync-key-lists-from-json --payload-stdin"
        ),
    };
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("trap 'touch {JOB_COMPLETION_FILE}' EXIT; {command}"),
    ]
}

fn build_exec_cmd(pod_name: &str, mode: ExecMode) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "-i".to_string(),
        pod_name.to_string(),
        "-c".to_string(),
        AWS_CONTAINER.to_string(),
        format!("--request-timeout={}s", KUBECTL_EXEC_TIMEOUT.as_secs()),
        "--".to_string(),
    ];
    args.extend(import_key_lists_command(mode));
    args
}

fn parse_sync_report(output: &str, expected: &[AwsKeyList]) -> Result<AwsSyncReport> {
    let report_line = output
        .lines()
        .find_map(|line| line.strip_prefix(RESULT_PREFIX))
        .ok_or_else(|| {
            anyhow!(
                "Remote output contained no AWS sync result: {}",
                output.trim()
            )
        })?;
    let report: AwsSyncReport =
        serde_json::from_str(report_line).context("Failed to parse remote AWS sync result")?;

    let expected_ids = expected
        .iter()
        .map(|item| item.id.as_str())
        .collect::<HashSet<_>>();
    if expected_ids.len() != expected.len() {
        bail!("AWS sync request contained duplicate key-list IDs");
    }
    let actual_ids = report
        .items
        .iter()
        .map(AwsSyncItemResult::id)
        .collect::<HashSet<_>>();
    if actual_ids.len() != report.items.len() {
        bail!("Remote AWS sync result contained duplicate key-list IDs");
    }
    if actual_ids != expected_ids {
        bail!(
            "Remote AWS sync result IDs did not match (expected: {expected_ids:?}, actual: {actual_ids:?})"
        );
    }
    Ok(report)
}

async fn ensure_eks_kubeconfig() -> Result<()> {
    let output = tokio::time::timeout(
        EKS_CONFIG_TIMEOUT,
        Command::new("cloud")
            .args([
                "eks",
                "update-kubeconfig",
                AWS_CLOUD,
                AWS_REGION,
                AWS_NAMESPACE,
            ])
            .output(),
    )
    .await
    .map_err(|_| anyhow!("cloud eks update-kubeconfig timed out"))?
    .context("Failed to run cloud CLI")?;

    if !output.status.success() {
        bail!(
            "cloud eks update-kubeconfig failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn ready_server_pod(pods: &PodList) -> Option<String> {
    pods.items.iter().find_map(|pod| {
        let server_is_ready = pod
            .status
            .as_ref()
            .and_then(|status| status.container_statuses.as_ref())
            .is_some_and(|statuses| {
                statuses
                    .iter()
                    .any(|status| status.name == AWS_CONTAINER && status.ready)
            });
        if server_is_ready {
            pod.metadata.name.clone()
        } else {
            None
        }
    })
}

async fn discover_aws_pod() -> Result<Option<String>> {
    let output = tokio::time::timeout(
        KUBECTL_TIMEOUT,
        Command::new("kubectl").args(build_discover_cmd()).output(),
    )
    .await
    .map_err(|_| anyhow!("kubectl pod discovery timed out"))?
    .context("Failed to run kubectl")?;

    if !output.status.success() {
        bail!(
            "kubectl get pods failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let pods: PodList = serde_json::from_slice(&output.stdout)
        .context("Failed to parse pods returned by kubectl")?;
    Ok(ready_server_pod(&pods))
}

async fn run_sync_via_pod(
    pod_name: &str,
    key_lists: &[AwsKeyList],
    mode: ExecMode,
) -> Result<String> {
    eprintln!("  → Executing sync in pod: {pod_name}");
    eprintln!("  → Syncing {} key list(s) to AWS...", key_lists.len());
    let payload = serde_json::to_vec(key_lists).context("Failed to serialize key lists")?;
    let output = tokio::time::timeout(KUBECTL_EXEC_PROCESS_TIMEOUT, async {
        let mut child = Command::new("kubectl")
            .args(build_exec_cmd(pod_name, mode))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Failed to run kubectl")?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open kubectl stdin"))?
            .write_all(&payload)
            .await
            .context("Failed to stream key lists to kubectl")?;
        child
            .wait_with_output()
            .await
            .context("kubectl exec failed")
    })
    .await
    .map_err(|_| anyhow!("kubectl exec timed out after {KUBECTL_EXEC_PROCESS_TIMEOUT:?}"))??;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!(
            "kubectl exec failed (stdout: {}; stderr: {})",
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(stdout.into_owned())
}

async fn kubectl_output(args: &[&str], timeout: Duration) -> Result<std::process::Output> {
    tokio::time::timeout(timeout, Command::new("kubectl").args(args).output())
        .await
        .map_err(|_| anyhow!("kubectl {} timed out", args.join(" ")))?
        .context("Failed to run kubectl")
}

async fn prepare_bootstrap_monad(job_name: &str) -> Result<BootstrapMonad> {
    let source = env::current_exe().context("Failed to locate the current monad binary")?;
    let bootstrap = BootstrapMonad {
        path: env::temp_dir().join(format!("{job_name}-mononoke-admin")),
    };
    eprintln!("  → Stripping debug information from the local monad binary...");
    let output = tokio::time::timeout(
        BOOTSTRAP_STRIP_TIMEOUT,
        Command::new("strip")
            .args(["--strip-debug", "-o"])
            .arg(&bootstrap.path)
            .arg(&source)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("Stripping the local monad binary timed out"))?
    .context("Failed to run strip on the local monad binary")?;
    if !output.status.success() {
        bail!(
            "Failed to strip the local monad binary: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut permissions = tokio::fs::metadata(&bootstrap.path)
        .await
        .context("Failed to inspect the stripped monad binary")?
        .permissions();
    permissions.set_mode(0o700);
    tokio::fs::set_permissions(&bootstrap.path, permissions)
        .await
        .context("Failed to make the stripped monad binary executable")?;
    Ok(bootstrap)
}

async fn upload_bootstrap_monad(pod_name: &str, bootstrap: &BootstrapMonad) -> Result<()> {
    let source = bootstrap
        .path
        .to_str()
        .ok_or_else(|| anyhow!("Bootstrap monad path is not valid UTF-8"))?;
    let destination = format!("{pod_name}:{BOOTSTRAP_MONAD_PATH}");
    eprintln!("  → Uploading the local monad binary to temporary pod {pod_name}...");
    let output = tokio::time::timeout(
        BOOTSTRAP_UPLOAD_TIMEOUT,
        Command::new("kubectl")
            .args(["cp", source, &destination, "-c", AWS_CONTAINER])
            .output(),
    )
    .await
    .map_err(|_| anyhow!("Uploading the local monad binary timed out"))?
    .context("Failed to run kubectl cp")?;
    if !output.status.success() {
        bail!(
            "Failed to upload the local monad binary: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let output = kubectl_output(
        &[
            "exec",
            pod_name,
            "-c",
            AWS_CONTAINER,
            "--",
            "chmod",
            "700",
            BOOTSTRAP_MONAD_PATH,
        ],
        KUBECTL_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        bail!(
            "Failed to make the uploaded monad binary executable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn deployment_template() -> Result<PodTemplateSpec> {
    let output = kubectl_output(
        &["get", "deployment", AWS_DEPLOYMENT, "-o", "json"],
        KUBECTL_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        bail!(
            "Failed to read AWS deployment: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let deployment: Deployment = serde_json::from_slice(&output.stdout)
        .context("Failed to parse AWS deployment returned by kubectl")?;
    Ok(deployment.spec.template)
}

fn build_job_manifest(
    job_name: &str,
    mut template: PodTemplateSpec,
    active_deadline_seconds: u64,
) -> Result<Job> {
    let template_spec = template
        .spec
        .as_mut()
        .ok_or_else(|| anyhow!("AWS deployment pod template has no spec"))?;
    let mut server = None;
    for container in std::mem::take(&mut template_spec.containers) {
        if container.name == AWS_CONTAINER {
            if server.is_some() {
                bail!("AWS deployment contains multiple '{AWS_CONTAINER}' containers");
            }
            server = Some(container);
        } else if container.name != AWS_LOG_TAILER_CONTAINER {
            bail!(
                "AWS deployment contains unsupported sibling container '{}'; refusing to remove a potential runtime dependency",
                container.name
            );
        }
    }
    if let Some(sidecar) = template_spec
        .init_containers
        .as_ref()
        .and_then(|containers| {
            containers
                .iter()
                .find(|container| container.restart_policy.as_deref() == Some("Always"))
        })
    {
        bail!(
            "AWS deployment contains native sidecar init container '{}'; refusing to create a job that may not finish",
            sidecar.name
        );
    }
    let mut server =
        server.ok_or_else(|| anyhow!("AWS deployment has no '{AWS_CONTAINER}' container"))?;
    server.command = Some(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("while [ ! -e {JOB_COMPLETION_FILE} ]; do sleep 1; done"),
    ]);
    server.args = Some(Vec::new());
    server.lifecycle = None;
    server.liveness_probe = None;
    server.readiness_probe = None;
    server.startup_probe = None;

    template
        .metadata
        .get_or_insert_with(Default::default)
        .labels
        .get_or_insert_with(Default::default)
        .insert(AWS_SYNC_JOB_LABEL.to_string(), "true".to_string());
    template_spec.containers = vec![server];
    template_spec.restart_policy = Some("Never".to_string());

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(job_name.to_string()),
            ..ObjectMeta::default()
        },
        spec: Some(JobSpec {
            active_deadline_seconds: Some(active_deadline_seconds as i64),
            backoff_limit: Some(0),
            template,
            ttl_seconds_after_finished: Some(JOB_TTL_SECONDS as i32),
            ..JobSpec::default()
        }),
        ..Job::default()
    })
}

async fn create_job(manifest: &Job) -> Result<()> {
    let manifest = serde_json::to_vec(manifest).context("Failed to serialize temporary AWS job")?;
    let mut child = Command::new("kubectl")
        .args(["create", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Failed to run kubectl")?;
    let output = tokio::time::timeout(KUBECTL_TIMEOUT, async {
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open kubectl stdin"))?
            .write_all(&manifest)
            .await
            .context("Failed to send temporary AWS job to kubectl")?;
        child
            .wait_with_output()
            .await
            .context("kubectl create job failed")
    })
    .await
    .map_err(|_| anyhow!("kubectl create job timed out"))??;
    if !output.status.success() {
        bail!(
            "Failed to create temporary AWS job: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn wait_for_job_pod(job_name: &str) -> Result<String> {
    let deadline = Instant::now() + JOB_START_TIMEOUT;
    loop {
        let selector = format!("job-name={job_name}");
        let output = kubectl_output(
            &["get", "pods", "-l", &selector, "-o", "json"],
            KUBECTL_TIMEOUT,
        )
        .await?;
        if !output.status.success() {
            bail!(
                "Failed to inspect temporary AWS job pod: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let pods: PodList = serde_json::from_slice(&output.stdout)
            .context("Failed to parse temporary AWS job pods returned by kubectl")?;
        if let Some(pod_name) = ready_server_pod(&pods) {
            return Ok(pod_name);
        }
        if Instant::now() >= deadline {
            bail!("Temporary AWS job pod did not start within {JOB_START_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn delete_job(job_name: &str) -> Result<()> {
    let output = kubectl_output(
        &[
            "delete",
            "job",
            job_name,
            "--ignore-not-found=true",
            "--wait=false",
        ],
        KUBECTL_TIMEOUT,
    )
    .await?;
    if !output.status.success() {
        bail!(
            "Failed to delete temporary AWS job: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn job_name_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let component = sanitized
        .trim_matches('-')
        .chars()
        .take(8)
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if component.is_empty() {
        "unknown".to_string()
    } else {
        component
    }
}

async fn run_temporary_job(
    key_lists: &[AwsKeyList],
    monad: TemporaryJobMonad,
) -> Result<AwsSyncReport> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_millis();
    let user = job_name_component(&env::var("USER").unwrap_or_else(|_| "unknown".to_string()));
    let hostname = hostname::get_hostname().context("Failed to get hostname")?;
    let short_hostname = hostname
        .split_once('.')
        .map_or(hostname.as_str(), |(short_hostname, _)| short_hostname);
    let hostname = job_name_component(short_hostname);
    let job_name = format!("redaction-sync-{user}-{hostname}-{timestamp}");
    let active_deadline_seconds = match monad {
        TemporaryJobMonad::Deployed => JOB_DEADLINE_SECONDS,
        TemporaryJobMonad::Bootstrap => BOOTSTRAP_JOB_DEADLINE_SECONDS,
    };
    let template = deployment_template().await?;
    let manifest = build_job_manifest(&job_name, template, active_deadline_seconds)?;
    let bootstrap = match monad {
        TemporaryJobMonad::Deployed => None,
        TemporaryJobMonad::Bootstrap => Some(prepare_bootstrap_monad(&job_name).await?),
    };

    eprintln!(
        "  → Creating one temporary job {job_name} for {} key list(s){}",
        key_lists.len(),
        match monad {
            TemporaryJobMonad::Deployed => "",
            TemporaryJobMonad::Bootstrap => " using the local monad binary",
        }
    );
    let result = async {
        create_job(&manifest).await?;
        let pod_name = wait_for_job_pod(&job_name).await?;
        if let Some(bootstrap) = bootstrap.as_ref() {
            upload_bootstrap_monad(&pod_name, bootstrap).await?;
        }
        let output = run_sync_via_pod(&pod_name, key_lists, ExecMode::TemporaryJob(monad)).await?;
        parse_sync_report(&output, key_lists)
    }
    .await;

    if let Err(error) = delete_job(&job_name).await {
        eprintln!(
            "  → Warning: {error}; Kubernetes cleanup is bounded by the {active_deadline_seconds}-second active deadline plus the {JOB_TTL_SECONDS}-second post-finish TTL"
        );
    }
    result
}

pub async fn prepare_sync(bootstrap_monad: bool) -> Result<AwsSyncTarget> {
    ensure_eks_kubeconfig().await?;
    if bootstrap_monad {
        return Ok(AwsSyncTarget::TemporaryJob(TemporaryJobMonad::Bootstrap));
    }
    Ok(match discover_aws_pod().await? {
        Some(pod_name) => AwsSyncTarget::RunningPod(pod_name),
        None => AwsSyncTarget::TemporaryJob(TemporaryJobMonad::Deployed),
    })
}

pub async fn sync_key_lists_to_aws(
    target: &AwsSyncTarget,
    key_lists: &[AwsKeyList],
) -> Result<(AwsSyncMethod, AwsSyncReport)> {
    let pod_name = match target {
        AwsSyncTarget::RunningPod(pod_name) => pod_name,
        AwsSyncTarget::TemporaryJob(monad) => {
            let report = run_temporary_job(key_lists, *monad).await?;
            return Ok((AwsSyncMethod::TemporaryJob, report));
        }
    };
    let running_pod_result = match run_sync_via_pod(pod_name, key_lists, ExecMode::RunningPod).await
    {
        Ok(output) => parse_sync_report(&output, key_lists),
        Err(error) => Err(error),
    };
    match running_pod_result {
        Ok(report) => Ok((AwsSyncMethod::RunningPod, report)),
        Err(original_error) => {
            eprintln!(
                "  → Running pod sync failed ({original_error:#}); retrying all key lists via one temporary job"
            );
            let report = run_temporary_job(key_lists, TemporaryJobMonad::Deployed).await?;
            Ok((AwsSyncMethod::TemporaryJob, report))
        }
    }
}

/// Best-effort sync used by `create-key-list`.
pub async fn sync_to_aws(keys: &[String], key_list_id: RedactionKeyListId) {
    eprintln!("\nChecking if sync to AWS is required...");
    let result = async {
        let target = prepare_sync(false).await?;
        let AwsSyncTarget::RunningPod(pod_name) = target else {
            bail!("No ready AWS Mononoke pod found");
        };
        let key_lists = [AwsKeyList {
            id: key_list_id.to_string(),
            keys: keys.to_vec(),
        }];
        let output = run_sync_via_pod(&pod_name, &key_lists, ExecMode::RunningPod).await?;
        parse_sync_report(&output, &key_lists)
    }
    .await;
    match result {
        Ok(report) if !report.has_failures() => {
            eprintln!("  → AWS sync complete via running pod (ID verified)");
        }
        Ok(report) => {
            let error = report
                .items
                .iter()
                .find_map(AwsSyncItemResult::error)
                .unwrap_or("unknown remote failure");
            eprintln!("  → Warning: {error}");
            eprintln!("  → Retry with: monad redaction sync-to-aws");
        }
        Err(error) => {
            eprintln!("  → Warning: {error}");
            eprintln!("  → Retry with: monad redaction sync-to-aws");
        }
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use serde_json::json;

    use super::*;

    #[mononoke::test]
    fn test_build_discover_cmd_includes_label_and_timeout() {
        let args = build_discover_cmd();
        assert!(args.contains(&"get".to_string()));
        assert!(args.contains(&"pods".to_string()));
        assert!(args.contains(&format!(
            "app.kubernetes.io/name=mononoke-server,!{AWS_SYNC_JOB_LABEL}"
        )));
        assert!(args.contains(&"--request-timeout=30s".to_string()));
        assert!(args.ends_with(&["-o".to_string(), "json".to_string()]));
    }

    #[mononoke::test]
    fn test_ready_server_pod_ignores_running_pod_without_server_container() {
        let pods: PodList = serde_json::from_value(json!({
            "items": [
                {
                    "metadata": { "name": "crash-looping" },
                    "status": {
                        "containerStatuses": [
                            {
                                "image": "tailer-image",
                                "imageID": "tailer-image-id",
                                "name": "tailer",
                                "ready": true,
                                "restartCount": 0,
                                "started": true,
                                "state": {}
                            }
                        ]
                    }
                },
                {
                    "metadata": { "name": "healthy" },
                    "status": {
                        "containerStatuses": [
                            {
                                "image": "server-image",
                                "imageID": "server-image-id",
                                "name": "server",
                                "ready": true,
                                "restartCount": 0,
                                "started": true,
                                "state": {}
                            }
                        ]
                    }
                }
            ]
        }))
        .expect("pod list must deserialize");
        assert_eq!(ready_server_pod(&pods).as_deref(), Some("healthy"));
    }

    #[mononoke::test]
    fn test_build_exec_cmd_reads_batch_from_stdin() {
        let args = build_exec_cmd("mononoke-pod-abc", ExecMode::RunningPod);

        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "-i");
        assert_eq!(args[2], "mononoke-pod-abc");
        assert!(args.contains(&"sync-key-lists-from-json".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("--payload-stdin"));
        assert!(!args.iter().any(|arg| arg.contains("key1")));
    }

    #[mononoke::test]
    fn test_temporary_job_exec_marks_completion() {
        let args = build_exec_cmd(
            "mononoke-job-pod",
            ExecMode::TemporaryJob(TemporaryJobMonad::Deployed),
        );
        let command = args.last().expect("exec command must be present");

        assert!(command.contains("sync-key-lists-from-json --payload-stdin"));
        assert!(command.contains(&format!("touch {JOB_COMPLETION_FILE}")));
    }

    #[mononoke::test]
    fn test_bootstrap_exec_uses_uploaded_binary_and_pod_config() {
        let args = build_exec_cmd(
            "mononoke-job-pod",
            ExecMode::TemporaryJob(TemporaryJobMonad::Bootstrap),
        );
        let command = args.last().expect("exec command must be present");

        assert!(command.contains(BOOTSTRAP_MONAD_PATH));
        assert!(command.contains("--acl-file /cfg/acls/acls"));
        assert!(command.contains("--config-tier aws --local-configerator-path /cfg"));
        assert!(command.contains("sync-key-lists-from-json --payload-stdin"));
        assert!(command.contains(&format!("touch {JOB_COMPLETION_FILE}")));
    }

    #[mononoke::test]
    fn test_parse_sync_report_validates_all_ids() -> Result<()> {
        let expected = vec![
            AwsKeyList {
                id: "id1".to_string(),
                keys: vec!["key1".to_string()],
            },
            AwsKeyList {
                id: "id2".to_string(),
                keys: vec!["key2".to_string()],
            },
        ];
        let output = concat!(
            "unrelated log line\n",
            "MONONOKE_AWS_SYNC_RESULT=",
            r#"{"items":[{"status":"already_present","id":"id1"},{"status":"inserted","id":"id2"}]}"#,
        );
        let report = parse_sync_report(output, &expected)?;
        assert_eq!(report.items.len(), 2);
        assert!(!report.has_failures());
        Ok(())
    }

    #[mononoke::test]
    fn test_job_name_component_is_dns_safe_and_bounded() {
        assert_eq!(job_name_component("Joald"), "joald");
        assert_eq!(job_name_component("devvm12345.example.com"), "devvm123");
        assert_eq!(job_name_component("abc-def-ghi"), "abc-def");
        assert_eq!(job_name_component("---devvm12345"), "devvm123");
        assert_eq!(job_name_component("---"), "unknown");
    }

    #[mononoke::test]
    fn test_job_manifest_has_cleanup_and_deadline() -> Result<()> {
        let template: PodTemplateSpec = serde_json::from_value(json!({
            "metadata": { "labels": { "app": "mononoke" } },
            "spec": {
                "serviceAccountName": "account",
                "restartPolicy": "Always",
                "containers": [
                    { "name": "server", "image": "server-image", "readinessProbe": {} },
                    { "name": "tailer", "image": "tailer-image" }
                ],
                "initContainers": [
                    { "name": "config-init", "image": "init-image" }
                ]
            }
        }))?;
        let bootstrap_manifest = build_job_manifest(
            "bootstrap-job",
            template.clone(),
            BOOTSTRAP_JOB_DEADLINE_SECONDS,
        )?;
        assert_eq!(
            bootstrap_manifest
                .spec
                .expect("bootstrap job must have a spec")
                .active_deadline_seconds,
            Some(BOOTSTRAP_JOB_DEADLINE_SECONDS as i64)
        );

        let manifest = build_job_manifest("sync-job", template, JOB_DEADLINE_SECONDS)?;
        let spec = manifest.spec.expect("job must have a spec");
        assert_eq!(
            spec.template
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.labels.as_ref())
                .and_then(|labels| labels.get(AWS_SYNC_JOB_LABEL))
                .map(String::as_str),
            Some("true")
        );
        let template_spec = spec.template.spec.expect("template must have a spec");

        assert_eq!(spec.backoff_limit, Some(0));
        assert_eq!(
            spec.active_deadline_seconds,
            Some(JOB_DEADLINE_SECONDS as i64)
        );
        assert_eq!(
            spec.ttl_seconds_after_finished,
            Some(JOB_TTL_SECONDS as i32)
        );
        assert_eq!(template_spec.restart_policy.as_deref(), Some("Never"));
        assert_eq!(
            template_spec.service_account_name.as_deref(),
            Some("account")
        );
        assert_eq!(template_spec.containers.len(), 1);
        let server = &template_spec.containers[0];
        assert_eq!(server.name, "server");
        assert_eq!(
            template_spec.init_containers.as_ref().unwrap()[0].name,
            "config-init"
        );
        assert!(server.readiness_probe.is_none());
        assert_eq!(server.args, Some(Vec::new()));
        assert_eq!(
            server.command,
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("while [ ! -e {JOB_COMPLETION_FILE} ]; do sleep 1; done"),
            ])
        );
        Ok(())
    }

    #[mononoke::test]
    fn test_job_manifest_rejects_unknown_sibling_container() {
        let template: PodTemplateSpec = serde_json::from_value(json!({
            "spec": {
                "containers": [
                    { "name": "server", "image": "server-image" },
                    { "name": "proxy", "image": "proxy-image" }
                ]
            }
        }))
        .expect("template must deserialize");
        let error = build_job_manifest("sync-job", template, JOB_DEADLINE_SECONDS)
            .expect_err("an unknown sibling container must be rejected");
        assert!(
            error
                .to_string()
                .contains("unsupported sibling container 'proxy'")
        );
    }
}
