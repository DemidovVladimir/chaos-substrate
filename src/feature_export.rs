use crate::{
    graph_export::GraphExport,
    obsidian_export::{write_obsidian_vault, ObsidianSummary},
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

const E2E_FEATURE_PAGE: &str = "e2e-encryption-feature-map.html";
const E2E_INFRA_PAGE: &str = "e2e-encryption-infra-map.html";

pub struct RefreshSummary {
    pub obsidian: ObsidianSummary,
    pub feature_pages: Vec<PathBuf>,
    pub skipped_feature_pages: Vec<PathBuf>,
}

pub fn refresh_project_exports(
    graph: &GraphExport,
    obsidian_output: &Path,
    features_dir: &Path,
    all_features: bool,
) -> Result<RefreshSummary> {
    let obsidian = write_obsidian_vault(obsidian_output, graph)?;
    fs::create_dir_all(features_dir)?;

    let mut feature_pages = Vec::new();
    let mut skipped_feature_pages = Vec::new();

    refresh_feature_page(
        graph,
        features_dir,
        E2E_FEATURE_PAGE,
        all_features,
        render_e2e_feature_page,
        &mut feature_pages,
        &mut skipped_feature_pages,
    )?;
    refresh_feature_page(
        graph,
        features_dir,
        E2E_INFRA_PAGE,
        all_features,
        render_e2e_infra_page,
        &mut feature_pages,
        &mut skipped_feature_pages,
    )?;

    Ok(RefreshSummary {
        obsidian,
        feature_pages,
        skipped_feature_pages,
    })
}

fn refresh_feature_page(
    graph: &GraphExport,
    features_dir: &Path,
    file_name: &str,
    all_features: bool,
    render: fn(&Path) -> Result<String>,
    written: &mut Vec<PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> Result<()> {
    let path = features_dir.join(file_name);
    if !all_features && !path.exists() {
        skipped.push(path);
        return Ok(());
    }
    let repo_root = Path::new(&graph.repository.root_path);
    fs::write(&path, render(repo_root)?)?;
    written.push(path);
    Ok(())
}

fn render_e2e_feature_page(repo_root: &Path) -> Result<String> {
    let claims = vec![
        claim(
            "ciphertext-uploaded",
            "Ciphertext is uploaded",
            "The browser replaces the original file with AES-GCM ciphertext before requesting upload finalization.",
            0.94,
            &["uploader", "encrypt-file", "ciphertext", "put-upload"],
        ),
        claim(
            "not-pure-e2e",
            "This is not pure E2E",
            "The backend can release the plaintext DEK after access-control evaluation, so the trust boundary includes KMS and resolver authorization.",
            0.9,
            &["access-check", "kms-key", "decrypt-hook"],
        ),
        claim(
            "metadata-drives-read",
            "Encryption metadata drives reads",
            "The encrypted DEK, IV, content hash, signer, timestamp, and access conditions are persisted as file metadata and used during decrypt.",
            0.92,
            &["metadata", "backend-finish", "read-router", "decrypt-hook"],
        ),
    ];
    let modes = vec![
        mode(
            "upload-flow",
            "Upload flow",
            &[
                "start-upload",
                "uploader",
                "generate-dek",
                "encrypt-file",
                "ciphertext",
                "put-upload",
                "finish-upload",
                "backend-finish",
            ],
        ),
        mode(
            "read-decrypt-flow",
            "Read/decrypt flow",
            &[
                "read-router",
                "decrypt-hook",
                "decrypt-data-key",
                "access-check",
                "kms-key",
                "decrypt-file",
            ],
        ),
        mode(
            "security-boundary",
            "Security boundary",
            &[
                "access-condition",
                "metadata",
                "backend-finish",
                "access-check",
                "kms-key",
            ],
        ),
        mode(
            "failure-modes",
            "Failure modes",
            &[
                "generate-dek",
                "encrypt-file",
                "initiate-upload",
                "put-upload",
                "finish-upload",
                "access-check",
            ],
        ),
    ];
    let nodes = vec![
        node(
            "start-upload",
            "Start upload",
            "UI",
            "ui",
            "desci-ecosystem/apps/labs/src/components/file-uploader/index.tsx",
            277,
            309,
            "Admin access starts the confidential upload path.",
        ),
        node(
            "uploader",
            "Upload lifecycle",
            "React hook",
            "ui",
            "desci-ecosystem/apps/labs/src/components/file-uploader/use-file-uploader.ts",
            61,
            155,
            "Orchestrates DEK generation, encryption, metadata, upload, and finalization.",
        ),
        node(
            "generate-dek",
            "Generate DEK",
            "SDK",
            "api",
            "desci-ecosystem/packages/client-sdk/src/domains/labs.ts",
            285,
            296,
            "Requests plaintext and encrypted DEK from the AppSync-backed Labs API.",
        ),
        node(
            "backend-generate",
            "KMS data key",
            "Lambda",
            "backend",
            "desci-infra/lambda/appsync-resolver-labs-lambda/resolvers/index.ts",
            444,
            487,
            "Generates a KMS-backed DEK and returns plaintext plus encrypted DEK.",
        ),
        node(
            "kms-key",
            "KMS key",
            "AWS",
            "infra",
            "desci-infra/lib/encryption-stack.ts",
            12,
            29,
            "Customer-managed key for data-room file envelope encryption.",
        ),
        node(
            "encrypt-file",
            "Encrypt file",
            "AES-GCM",
            "crypto",
            "desci-ecosystem/packages/storage/src/lib/encryption/kms-envelope.ts",
            45,
            70,
            "Browser encrypts plaintext bytes before upload.",
        ),
        node(
            "ciphertext",
            "Ciphertext file",
            "payload",
            "data",
            "desci-ecosystem/apps/labs/src/components/file-uploader/use-file-uploader.ts",
            84,
            91,
            "Upload payload is replaced with ciphertext bytes.",
        ),
        node(
            "access-condition",
            "Access condition",
            "IPNFT signer",
            "crypto",
            "desci-ecosystem/packages/storage/src/lib/access-control.ts",
            25,
            106,
            "Builds the EVM condition checked before DEK release.",
        ),
        node(
            "metadata",
            "Encryption metadata",
            "encryptedDek + ACL",
            "data",
            "desci-ecosystem/apps/labs/src/components/file-uploader/use-file-uploader.ts",
            107,
            115,
            "Carries encrypted DEK, IV, content hash, author, timestamp, and access policy.",
        ),
        node(
            "initiate-upload",
            "Initiate upload",
            "signed URL",
            "api",
            "desci-ecosystem/apps/labs/src/components/file-uploader/utils.ts",
            12,
            25,
            "Uses ciphertext size to request an upload URL.",
        ),
        node(
            "put-upload",
            "PUT ciphertext",
            "Kamu/S3",
            "api",
            "desci-ecosystem/apps/labs/src/components/file-uploader/utils.ts",
            27,
            90,
            "Uploads ciphertext with returned headers.",
        ),
        node(
            "finish-upload",
            "Finish upload",
            "metadata",
            "api",
            "desci-ecosystem/apps/labs/src/components/file-uploader/utils.ts",
            108,
            145,
            "Finalizes the data-room file with ADMIN access and encryption metadata.",
        ),
        node(
            "backend-finish",
            "Backend finish",
            "validate + persist",
            "backend",
            "desci-infra/lambda/appsync-resolver-labs-lambda/resolvers/index.ts",
            680,
            790,
            "Parses, validates, and persists metadata through Kamu.",
        ),
        node(
            "read-router",
            "Read router",
            "useDecryptedFile",
            "read",
            "desci-ecosystem/packages/storage/src/hooks/use-decrypted-file.ts",
            29,
            55,
            "Routes KMS metadata to the KMS decryption hook.",
        ),
        node(
            "decrypt-hook",
            "Decrypt hook",
            "useKmsDecryption",
            "read",
            "desci-ecosystem/packages/storage/src/hooks/decryption/use-kms-decryption.ts",
            21,
            52,
            "Fetches ciphertext and requests DEK release in parallel.",
        ),
        node(
            "decrypt-data-key",
            "Decrypt DEK",
            "SDK",
            "api",
            "desci-ecosystem/packages/client-sdk/src/domains/labs.ts",
            298,
            330,
            "Requests plaintext DEK for an authorized file read.",
        ),
        node(
            "access-check",
            "Access check",
            "resolver",
            "backend",
            "desci-infra/lambda/appsync-resolver-labs-lambda/resolvers/index.ts",
            489,
            670,
            "Rejects legacy Lit, evaluates conditions, and unwraps the DEK.",
        ),
        node(
            "decrypt-file",
            "Decrypt file",
            "AES-GCM",
            "crypto",
            "desci-ecosystem/packages/storage/src/lib/encryption/kms-envelope.ts",
            72,
            87,
            "Browser decrypts ciphertext to a local blob URL.",
        ),
    ];
    let edges = vec![
        edge(
            "start-upload",
            "uploader",
            "passes confidential flag",
            "control-flow",
        ),
        edge("uploader", "generate-dek", "requests key", "call-flow"),
        edge("generate-dek", "backend-generate", "GraphQL", "call-flow"),
        edge(
            "backend-generate",
            "kms-key",
            "GenerateDataKey",
            "infra-call",
        ),
        edge("uploader", "encrypt-file", "plaintext + DEK", "data-flow"),
        edge(
            "encrypt-file",
            "ciphertext",
            "ciphertext bytes",
            "data-flow",
        ),
        edge("access-condition", "metadata", "policy JSON", "data-flow"),
        edge("generate-dek", "metadata", "encryptedDek", "data-flow"),
        edge("encrypt-file", "metadata", "iv + contentHash", "data-flow"),
        edge("ciphertext", "initiate-upload", "size", "data-flow"),
        edge(
            "initiate-upload",
            "put-upload",
            "URL + headers",
            "call-flow",
        ),
        edge("metadata", "finish-upload", "metadata", "data-flow"),
        edge("finish-upload", "backend-finish", "finalize", "call-flow"),
        edge(
            "backend-finish",
            "read-router",
            "stored metadata",
            "storage-flow",
        ),
        edge(
            "read-router",
            "decrypt-hook",
            "protocol kms",
            "control-flow",
        ),
        edge(
            "decrypt-hook",
            "decrypt-data-key",
            "request DEK",
            "call-flow",
        ),
        edge("decrypt-data-key", "access-check", "GraphQL", "call-flow"),
        edge("access-check", "kms-key", "Decrypt", "infra-call"),
        edge("access-check", "decrypt-hook", "plaintextDEK", "data-flow"),
        edge(
            "decrypt-hook",
            "decrypt-file",
            "ciphertext + key",
            "data-flow",
        ),
    ];
    render_blade_runner_page(PageSpec {
        feature: feature(
            "e2e-encryption",
            "Encrypted Data Room",
            "security",
            "KMS envelope encryption for confidential data-room upload and read paths.",
        ),
        title: "Encrypted Data Room Feature Map",
        subtitle:
            "A focused feature graph regenerated from the current Chaos Substrate index and source tree.",
        companion_href: "e2e-encryption-infra-map.html",
        companion_label: "Open infrastructure correlation map",
        repo_root,
        claims,
        modes,
        nodes,
        edges,
        story: &[
            "Admin upload starts confidential mode",
            "Client asks backend/KMS for a DEK",
            "Browser encrypts before upload",
            "Ciphertext and metadata are finalized",
            "Read path evaluates access before DEK release",
            "Browser decrypts locally",
        ],
    })
}

fn render_e2e_infra_page(repo_root: &Path) -> Result<String> {
    let claims = vec![
        claim(
            "kms-key-wired",
            "KMS key is wired through infrastructure",
            "CDK passes the customer-managed key into the Labs resolver Lambda and grants GenerateDataKey/Decrypt.",
            0.93,
            &["encryption-stack", "appsync-stack", "kms-grants", "lambda-env", "runtime"],
        ),
        claim(
            "client-contract-wraps-api",
            "Client code uses generated GraphQL contracts",
            "The SDK wraps GraphQL documents for upload, key generation, and key release before React calls the SDK.",
            0.88,
            &["graphql", "sdk", "react-upload", "react-read"],
        ),
    ];
    let modes = vec![
        mode(
            "architecture",
            "Architecture",
            &[
                "encryption-stack",
                "appsync-stack",
                "iphubs-call",
                "iphubs-props",
                "runtime",
                "sdk",
            ],
        ),
        mode(
            "infra-to-runtime",
            "Infrastructure to runtime",
            &[
                "encryption-stack",
                "kms-grants",
                "lambda-env",
                "deps",
                "resolvers",
                "runtime",
            ],
        ),
        mode(
            "client-contract",
            "Client contract",
            &["resolvers", "graphql", "sdk", "react-upload", "react-read"],
        ),
        mode(
            "security-boundary",
            "Security boundary",
            &["kms-grants", "lambda-env", "runtime", "graphql", "sdk"],
        ),
    ];
    let nodes = vec![
        node("encryption-stack", "EncryptionStack", "CDK", "infra", "desci-infra/lib/encryption-stack.ts", 12, 29, "Defines the FileEncryptionKey CMK."),
        node("appsync-stack", "DesciAppSyncStack", "CDK props", "infra", "desci-infra/lib/desci-api-app-sync/index.ts", 25, 44, "Stack contract receives fileEncryptionKey."),
        node("iphubs-call", "ip-hubs config", "construct", "infra", "desci-infra/lib/desci-api-app-sync/index.ts", 202, 223, "Passes the KMS key into AppSyncLabsLambdaConfig."),
        node("iphubs-props", "IPHubs props", "typed deps", "infra", "desci-infra/lib/desci-api-app-sync/constructs/appsync-labs-lambda-config.ts", 20, 40, "Declares runtime dependencies including fileEncryptionKey."),
        node("kms-grants", "KMS grants", "IAM", "infra", "desci-infra/lib/desci-api-app-sync/constructs/appsync-labs-lambda-config.ts", 146, 151, "Grants GenerateDataKey and Decrypt to the resolver Lambda."),
        node("lambda-env", "Lambda env", "KMS_CMK_ARN", "backend", "desci-infra/lib/desci-api-app-sync/constructs/appsync-labs-lambda-config.ts", 235, 308, "Injects the CMK ARN into resolver runtime."),
        node("deps", "Secrets + DB", "runtime deps", "backend", "desci-infra/lib/desci-api-app-sync/constructs/appsync-labs-lambda-config.ts", 310, 341, "Grants secrets, Aurora, and service token table access."),
        node("resolvers", "Resolver registration", "GraphQL", "backend", "desci-infra/lib/desci-api-app-sync/constructs/appsync-labs-lambda-config.ts", 346, 390, "Registers encryption mutations on the Lambda."),
        node("runtime", "DataRoomResolvers", "runtime", "backend", "desci-infra/lambda/appsync-resolver-labs-lambda/resolvers/index.ts", 444, 670, "Uses KMS service to generate and decrypt DEKs."),
        node("graphql", "GraphQL documents", "client contract", "api", "desci-ecosystem/packages/client-sdk/src/graphql/documents/desci-api/mutations/ip-hubs.ts", 13, 164, "Client operation definitions for upload and encryption mutations."),
        node("sdk", "LabsApi SDK", "facade", "api", "desci-ecosystem/packages/client-sdk/src/domains/labs.ts", 185, 330, "Typed frontend-facing API surface."),
        node("react-upload", "React upload", "client", "ui", "desci-ecosystem/apps/labs/src/components/file-uploader/use-file-uploader.ts", 61, 155, "Consumes SDK to encrypt and upload confidential files."),
        node("react-read", "React read", "client", "read", "desci-ecosystem/packages/storage/src/hooks/decryption/use-kms-decryption.ts", 21, 52, "Consumes SDK to request DEK and decrypt locally."),
    ];
    let edges = vec![
        edge(
            "encryption-stack",
            "appsync-stack",
            "key becomes stack prop",
            "infra-flow",
        ),
        edge(
            "appsync-stack",
            "iphubs-call",
            "passed to construct",
            "infra-flow",
        ),
        edge(
            "iphubs-call",
            "iphubs-props",
            "typed dependency",
            "infra-flow",
        ),
        edge(
            "iphubs-props",
            "kms-grants",
            "IAM grant",
            "security-boundary",
        ),
        edge("iphubs-props", "lambda-env", "KMS_CMK_ARN", "config-flow"),
        edge("kms-grants", "runtime", "KMS access", "security-boundary"),
        edge("lambda-env", "runtime", "runtime config", "config-flow"),
        edge("deps", "runtime", "secrets + DB", "runtime-dependency"),
        edge("resolvers", "runtime", "routes mutations", "call-flow"),
        edge("resolvers", "graphql", "same fields", "contract-flow"),
        edge("graphql", "sdk", "wrapped documents", "contract-flow"),
        edge("sdk", "react-upload", "upload calls", "call-flow"),
        edge("sdk", "react-read", "read calls", "call-flow"),
    ];
    render_blade_runner_page(PageSpec {
        feature: feature(
            "e2e-encryption",
            "Encrypted Data Room",
            "security",
            "KMS envelope encryption for confidential data-room upload and read paths.",
        ),
        title: "Infrastructure Correlation Map",
        subtitle:
            "How AWS CDK and CloudFormation wiring becomes the React encrypted data-room feature.",
        companion_href: "e2e-encryption-feature-map.html",
        companion_label: "Back to feature flow map",
        repo_root,
        claims,
        modes,
        nodes,
        edges,
        story: &[
            "CDK creates the KMS key",
            "AppSync stack passes key to the Labs Lambda",
            "Lambda receives KMS_CMK_ARN and IAM grants",
            "GraphQL registers encryption mutations",
            "SDK wraps those mutations",
            "React uses the SDK for upload and read",
        ],
    })
}

#[derive(Serialize)]
struct FeatureNode {
    id: &'static str,
    label: &'static str,
    subtitle: &'static str,
    group: &'static str,
    file: &'static str,
    lines: String,
    role: &'static str,
    code: String,
    evidence: FeatureEvidence,
    confidence: f32,
}

#[derive(Serialize)]
struct FeatureEdge {
    source: &'static str,
    target: &'static str,
    label: &'static str,
    kind: &'static str,
    evidence: FeatureEvidence,
    confidence: f32,
}

#[derive(Serialize)]
struct FeatureManifest<'a> {
    schema_version: &'static str,
    feature: FeatureDefinition<'a>,
    title: &'a str,
    subtitle: &'a str,
    claims: &'a [FeatureClaim],
    modes: &'a [FeatureMode],
    nodes: &'a [FeatureNode],
    edges: &'a [FeatureEdge],
    story: &'a [&'a str],
}

#[derive(Clone, Copy, Serialize)]
struct FeatureDefinition<'a> {
    id: &'a str,
    title: &'a str,
    domain: &'a str,
    summary: &'a str,
}

#[derive(Serialize)]
struct FeatureClaim {
    id: &'static str,
    title: &'static str,
    body: &'static str,
    confidence: f32,
    node_ids: Vec<&'static str>,
}

#[derive(Serialize)]
struct FeatureMode {
    id: &'static str,
    title: &'static str,
    node_ids: Vec<&'static str>,
}

#[derive(Default, Serialize)]
struct FeatureEvidence {
    source: &'static str,
    method: &'static str,
    notes: &'static str,
}

#[allow(clippy::too_many_arguments)]
fn node(
    id: &'static str,
    label: &'static str,
    subtitle: &'static str,
    group: &'static str,
    file: &'static str,
    start: usize,
    end: usize,
    role: &'static str,
) -> FeatureNode {
    FeatureNode {
        id,
        label,
        subtitle,
        group,
        file,
        lines: format!("{start}-{end}"),
        role,
        code: String::new(),
        evidence: FeatureEvidence {
            source: "source-snippet",
            method: "manual-feature-map",
            notes: "Source range is read from the current repository during refresh.",
        },
        confidence: 0.9,
    }
}

fn edge(
    source: &'static str,
    target: &'static str,
    label: &'static str,
    kind: &'static str,
) -> FeatureEdge {
    FeatureEdge {
        source,
        target,
        label,
        kind,
        evidence: FeatureEvidence {
            source: "feature-map",
            method: "manual-feature-map",
            notes: "Relationship is curated from source inspection and persisted feature context.",
        },
        confidence: 0.86,
    }
}

fn feature<'a>(
    id: &'a str,
    title: &'a str,
    domain: &'a str,
    summary: &'a str,
) -> FeatureDefinition<'a> {
    FeatureDefinition {
        id,
        title,
        domain,
        summary,
    }
}

fn claim(
    id: &'static str,
    title: &'static str,
    body: &'static str,
    confidence: f32,
    node_ids: &[&'static str],
) -> FeatureClaim {
    FeatureClaim {
        id,
        title,
        body,
        confidence,
        node_ids: node_ids.to_vec(),
    }
}

fn mode(id: &'static str, title: &'static str, node_ids: &[&'static str]) -> FeatureMode {
    FeatureMode {
        id,
        title,
        node_ids: node_ids.to_vec(),
    }
}

struct PageSpec<'a> {
    feature: FeatureDefinition<'a>,
    title: &'a str,
    subtitle: &'a str,
    companion_href: &'a str,
    companion_label: &'a str,
    repo_root: &'a Path,
    claims: Vec<FeatureClaim>,
    modes: Vec<FeatureMode>,
    nodes: Vec<FeatureNode>,
    edges: Vec<FeatureEdge>,
    story: &'a [&'a str],
}

fn render_blade_runner_page(mut spec: PageSpec<'_>) -> Result<String> {
    for node in &mut spec.nodes {
        node.code = read_snippet(spec.repo_root, node.file, &node.lines)
            .unwrap_or_else(|err| format!("Unable to read snippet: {err}"));
    }
    let nodes_json = serde_json::to_string(&spec.nodes)?;
    let edges_json = serde_json::to_string(&spec.edges)?;
    let story_json = serde_json::to_string(spec.story)?;
    let claims_json = serde_json::to_string(&spec.claims)?;
    let modes_json = serde_json::to_string(&spec.modes)?;
    let manifest_json = serde_json::to_string(&FeatureManifest {
        schema_version: "1",
        feature: spec.feature,
        title: spec.title,
        subtitle: spec.subtitle,
        claims: &spec.claims,
        modes: &spec.modes,
        nodes: &spec.nodes,
        edges: &spec.edges,
        story: spec.story,
    })?;
    Ok(FEATURE_HTML
        .replace("__TITLE__", spec.title)
        .replace("__SUBTITLE__", spec.subtitle)
        .replace("__COMPANION_HREF__", spec.companion_href)
        .replace("__COMPANION_LABEL__", spec.companion_label)
        .replace("__MANIFEST__", &escape_script_json(&manifest_json))
        .replace("__NODES__", &escape_script_json(&nodes_json))
        .replace("__EDGES__", &escape_script_json(&edges_json))
        .replace("__CLAIMS__", &escape_script_json(&claims_json))
        .replace("__MODES__", &escape_script_json(&modes_json))
        .replace("__STORY__", &escape_script_json(&story_json)))
}

fn read_snippet(repo_root: &Path, file: &str, lines: &str) -> Result<String> {
    let path = repo_root.join(file);
    let content =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let (start, end) = parse_line_range(lines)?;
    let mut out = String::new();
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        if (start..=end).contains(&line_no) {
            out.push_str(&format!("{line_no:>4}  {line}\n"));
        }
    }
    Ok(out)
}

fn parse_line_range(value: &str) -> Result<(usize, usize)> {
    let (start, end) = value.split_once('-').unwrap_or((value, value));
    Ok((start.parse()?, end.parse()?))
}

fn escape_script_json(json: &str) -> String {
    json.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

const FEATURE_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
:root{--bg:#07080d;--panel:#10131d;--panel2:#151927;--ink:#f5f7fb;--muted:#8d9ab8;--line:#293047;--cyan:#32e6ff;--pink:#ff3d9a;--amber:#ffb000;--green:#3cff98;--red:#ff5a4e;--violet:#9f6bff}
*{box-sizing:border-box} body{margin:0;background:radial-gradient(circle at 18% 0%,rgba(50,230,255,.18),transparent 28%),radial-gradient(circle at 84% 14%,rgba(255,61,154,.16),transparent 24%),linear-gradient(180deg,#090a12,#05060a);color:var(--ink);font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
header{padding:30px 34px 20px;border-bottom:1px solid var(--line);background:linear-gradient(90deg,rgba(16,19,29,.92),rgba(16,19,29,.72));box-shadow:0 20px 70px rgba(0,0,0,.45)}
h1{margin:0 0 8px;font-size:clamp(30px,4vw,52px);letter-spacing:0;text-shadow:0 0 28px rgba(50,230,255,.28)} a{color:var(--cyan);font-weight:800;text-decoration:none}.subtitle{max-width:1100px;color:var(--muted);line-height:1.55}
.layout{display:grid;grid-template-columns:minmax(440px,1.3fr) minmax(360px,.7fr);gap:18px;padding:18px}
.panel,.sidebox{background:linear-gradient(180deg,rgba(21,25,39,.96),rgba(12,14,22,.96));border:1px solid var(--line);border-radius:8px;box-shadow:0 22px 80px rgba(0,0,0,.45),0 0 34px rgba(50,230,255,.08)}
.toolbar{display:flex;justify-content:space-between;gap:12px;padding:14px 16px;border-bottom:1px solid var(--line)}button{border:1px solid var(--line);background:#0b0e16;color:var(--ink);border-radius:7px;padding:8px 10px;font-weight:750;cursor:pointer}button:hover,button.active{border-color:var(--cyan);color:var(--cyan);box-shadow:0 0 18px rgba(50,230,255,.16)}
.legend{display:flex;flex-wrap:wrap;gap:8px 12px;color:var(--muted);font-size:13px}.legend span{display:inline-flex;gap:6px;align-items:center}.dot{width:10px;height:10px;border-radius:50%;display:inline-block}
svg{width:100%;min-height:690px;display:block;background:linear-gradient(rgba(50,230,255,.04) 1px,transparent 1px),linear-gradient(90deg,rgba(255,61,154,.04) 1px,transparent 1px);background-size:30px 30px}
.edge{stroke:#59627a;stroke-width:2;fill:none;marker-end:url(#arrow);opacity:.86}.edge.active{stroke:var(--cyan);stroke-width:3.5}.edge.dim{opacity:.16}.edge-label{font-size:11px;fill:#b7c1dd;paint-order:stroke;stroke:#07080d;stroke-width:4;stroke-linejoin:round}
.node rect{stroke-width:2;rx:8;filter:drop-shadow(0 0 15px rgba(50,230,255,.13));cursor:pointer}.node text{pointer-events:none;font-size:13px;fill:var(--ink);font-weight:800}.node .small{font-size:11px;fill:var(--muted);font-weight:700}.node.active rect{stroke:#fff;stroke-width:3;filter:drop-shadow(0 0 22px rgba(50,230,255,.42))}.node.dim{opacity:.26}
.right{display:grid;gap:18px;grid-template-rows:auto auto 1fr}.sidebox{padding:16px}.sidebox h2{margin:0 0 10px;font-size:18px}.steps,.claims,.modes{display:grid;gap:8px}.steps button,.modes button{text-align:left;line-height:1.35;background:#0b0e16}.claim{padding:10px;border:1px solid var(--line);border-radius:7px;background:#0b0e16}.claim strong{color:var(--cyan)}.claim small{display:block;color:var(--muted);margin-top:6px}
.inspector{padding:0;overflow:hidden}.head{padding:16px;border-bottom:1px solid var(--line)}.badge{display:inline-flex;border-radius:999px;color:#05060a;padding:5px 9px;font-size:12px;font-weight:900;margin-bottom:10px;background:var(--cyan)}.meta{color:var(--muted);font-size:13px;line-height:1.45;overflow-wrap:anywhere}.body{padding:15px 16px 16px;overflow:auto;max-height:610px}.explain{line-height:1.55}.section{color:var(--muted);font-size:12px;font-weight:900;text-transform:uppercase;letter-spacing:.05em;margin-top:14px}.relation{padding:9px 10px;border:1px solid var(--line);border-radius:7px;background:#0b0e16;font-size:13px;margin-top:8px;cursor:pointer}.relation strong{color:var(--cyan)}
pre{margin:10px 0 0;padding:14px;border-radius:8px;background:#030409;color:#d8e2ff;overflow:auto;font-size:12px;line-height:1.48;border:1px solid #242a3d}
@media(max-width:1050px){.layout{grid-template-columns:1fr}svg{min-height:600px}}@media(max-width:640px){header{padding:22px 18px 16px}.layout{padding:10px}.toolbar{align-items:flex-start;flex-direction:column}}
</style>
</head>
<body>
<header><h1>__TITLE__</h1><div class="subtitle">__SUBTITLE__<br><a href="__COMPANION_HREF__">__COMPANION_LABEL__</a></div></header>
<main class="layout">
<section class="panel"><div class="toolbar"><div class="legend"><span><i class="dot" style="background:var(--cyan)"></i>api/read</span><span><i class="dot" style="background:var(--green)"></i>ui/crypto</span><span><i class="dot" style="background:var(--amber)"></i>infra</span><span><i class="dot" style="background:var(--red)"></i>backend</span><span><i class="dot" style="background:var(--violet)"></i>data</span></div><button id="resetBtn">Reset</button></div><svg id="graph" viewBox="0 0 1180 760"><defs><marker id="arrow" markerWidth="10" markerHeight="10" refX="9" refY="3" orient="auto" markerUnits="strokeWidth"><path d="M0,0 L0,6 L9,3 z" fill="#59627a"></path></marker></defs></svg></section>
<aside class="right"><section class="sidebox"><h2>Claims</h2><div id="claims" class="claims"></div></section><section class="sidebox"><h2>Modes</h2><div id="modes" class="modes"></div></section><section class="sidebox"><h2>Regenerated Flow</h2><div id="steps" class="steps"></div></section><section class="sidebox inspector"><div class="head"><span id="badge" class="badge">node</span><h2 id="title">Select a node</h2><div id="meta" class="meta"></div></div><div id="body" class="body"></div></section></aside>
</main>
<script type="application/json" id="chaos-feature-manifest">__MANIFEST__</script>
<script>
const nodes=__NODES__;const edges=__EDGES__;const story=__STORY__;const claims=__CLAIMS__;const modes=__MODES__;
const colors={ui:"#3cff98",crypto:"#3cff98",api:"#32e6ff",read:"#32e6ff",backend:"#ff5a4e",infra:"#ffb000",data:"#9f6bff"};
const graph=document.getElementById("graph"),byId=Object.fromEntries(nodes.map((n,i)=>[n.id,{...n,x:70+(i%5)*220,y:70+Math.floor(i/5)*165,w:170,h:74}]));let active=null;
function esc(v){return String(v).replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;").replaceAll('"',"&quot;").replaceAll("'","&#039;")}
function center(n){return{x:n.x+n.w/2,y:n.y+n.h/2}}function edgePath(a,b){const ac=center(a),bc=center(b),dx=Math.max(45,Math.abs(bc.x-ac.x)*.38);return`M ${ac.x} ${ac.y} C ${ac.x+dx} ${ac.y}, ${bc.x-dx} ${bc.y}, ${bc.x} ${bc.y}`}
function draw(){const defs=graph.querySelector("defs");graph.innerHTML="";graph.appendChild(defs);edges.forEach(e=>{const p=document.createElementNS("http://www.w3.org/2000/svg","path");p.setAttribute("class","edge");p.setAttribute("d",edgePath(byId[e.source],byId[e.target]));p.dataset.source=e.source;p.dataset.target=e.target;graph.appendChild(p);const ac=center(byId[e.source]),bc=center(byId[e.target]);const t=document.createElementNS("http://www.w3.org/2000/svg","text");t.setAttribute("class","edge-label");t.setAttribute("x",(ac.x+bc.x)/2);t.setAttribute("y",(ac.y+bc.y)/2-8);t.setAttribute("text-anchor","middle");t.textContent=e.label;graph.appendChild(t)});Object.values(byId).forEach(n=>{const g=document.createElementNS("http://www.w3.org/2000/svg","g");g.setAttribute("class","node");g.dataset.id=n.id;g.setAttribute("tabindex","0");const r=document.createElementNS("http://www.w3.org/2000/svg","rect");r.setAttribute("x",n.x);r.setAttribute("y",n.y);r.setAttribute("width",n.w);r.setAttribute("height",n.h);r.setAttribute("fill","#10131d");r.setAttribute("stroke",colors[n.group]||"#32e6ff");g.appendChild(r);const band=document.createElementNS("http://www.w3.org/2000/svg","rect");band.setAttribute("x",n.x);band.setAttribute("y",n.y);band.setAttribute("width",8);band.setAttribute("height",n.h);band.setAttribute("rx",7);band.setAttribute("fill",colors[n.group]||"#32e6ff");g.appendChild(band);const t1=document.createElementNS("http://www.w3.org/2000/svg","text");t1.setAttribute("x",n.x+18);t1.setAttribute("y",n.y+31);t1.textContent=n.label;g.appendChild(t1);const t2=document.createElementNS("http://www.w3.org/2000/svg","text");t2.setAttribute("x",n.x+18);t2.setAttribute("y",n.y+53);t2.setAttribute("class","small");t2.textContent=n.subtitle;g.appendChild(t2);g.onclick=()=>select(n.id);g.onkeydown=e=>{if(e.key==="Enter"||e.key===" ")select(n.id)};graph.appendChild(g)})}
function relations(id){let rows=[];edges.forEach(e=>{if(e.source===id)rows.push(["To",byId[e.target],e.label]);if(e.target===id)rows.push(["From",byId[e.source],e.label])});return rows.map(r=>`<div class="relation" data-target="${r[1].id}"><strong>${r[0]} ${esc(r[1].label)}</strong><br>${esc(r[2])}</div>`).join("")||'<div class="meta">No direct relations.</div>'}
function select(id){active=id;const n=byId[id];document.getElementById("title").textContent=n.label;document.getElementById("meta").textContent=`${n.file} | lines ${n.lines}`;const b=document.getElementById("badge");b.textContent=n.group;b.style.background=colors[n.group]||"#32e6ff";document.getElementById("body").innerHTML=`<div class="explain">${esc(n.role)}</div><div class="section">Evidence</div><div class="meta">${esc(n.evidence.method)} | ${esc(n.evidence.source)} | confidence ${Math.round((n.confidence||0)*100)}%</div><div class="section">Relations</div>${relations(id)}<div class="section">Source</div><pre><code>${esc(n.code)}</code></pre>`;document.querySelectorAll(".relation").forEach(el=>el.onclick=()=>select(el.dataset.target));update(new Set([id]))}
function update(focus){const related=new Set(focus||[active]);edges.forEach(e=>{if(related.has(e.source))related.add(e.target);if(related.has(e.target))related.add(e.source)});document.querySelectorAll(".node").forEach(g=>{g.classList.toggle("active",active&&related.has(g.dataset.id));g.classList.toggle("dim",active&&!related.has(g.dataset.id))});document.querySelectorAll(".edge").forEach(e=>{const on=related.has(e.dataset.source)&&related.has(e.dataset.target);e.classList.toggle("active",on);e.classList.toggle("dim",active&&!on)});document.querySelectorAll("#steps button").forEach((b,i)=>b.classList.toggle("active",Object.keys(byId)[i]===active))}
function steps(){const root=document.getElementById("steps");story.forEach((s,i)=>{const id=Object.keys(byId)[Math.min(i,Object.keys(byId).length-1)];const b=document.createElement("button");b.innerHTML=`<strong>${i+1}.</strong> ${esc(s)}`;b.onclick=()=>select(id);root.appendChild(b)})}
function claimCards(){const root=document.getElementById("claims");claims.forEach(c=>{const el=document.createElement("div");el.className="claim";el.innerHTML=`<strong>${esc(c.title)}</strong><br>${esc(c.body)}<small>confidence ${Math.round((c.confidence||0)*100)}%</small>`;el.onclick=()=>{active=c.node_ids[0];update(new Set(c.node_ids))};root.appendChild(el)})}
function modeButtons(){const root=document.getElementById("modes");modes.forEach(m=>{const b=document.createElement("button");b.textContent=m.title;b.onclick=()=>{active=m.node_ids[0];update(new Set(m.node_ids))};root.appendChild(b)})}
document.getElementById("resetBtn").onclick=()=>select(Object.keys(byId)[0]);draw();claimCards();modeButtons();steps();select(Object.keys(byId)[0]);
</script></body></html>"##;

#[cfg(test)]
mod tests {
    use super::parse_line_range;

    #[test]
    fn parses_line_ranges() {
        assert_eq!(parse_line_range("12-29").unwrap(), (12, 29));
        assert_eq!(parse_line_range("7").unwrap(), (7, 7));
    }
}
