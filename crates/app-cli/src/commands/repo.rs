//! `rl repo` and `rl worktree` dispatch, plus the repo-handle resolvers and
//! path helpers they share. `DiscoveredRepo` is re-exported from the crate
//! root and referenced by `render.rs`.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use dto_shared::{AttachRepoCmd, LinkWorktreeCmd, LocateResponseDto, UnlinkWorktreeCmd};
use infra_filesystem::{TokioFilesystemProbe, discover_repos_under};
use infra_git::discover_canonical;

use crate::cli::{AliasArg, BranchArg, RepoAliasCmd, RepoCmd, WorkspaceArg, WorktreeCmd};
use crate::commands::handle_ambiguous;
use crate::render;
use crate::services::Services;

pub(crate) async fn repo_dispatch(cmd: RepoCmd, svc: &Services) -> Result<()> {
    match cmd {
        RepoCmd::Attach {
            ws: WorkspaceArg { workspace },
            url,
            canonical,
            br: BranchArg { branch },
            path,
            no_link,
            prefix,
        } => {
            let link_path = resolve_attach_link_path(path.as_deref(), no_link, &canonical)?;

            let outcome = svc
                .bindings
                .attach(AttachRepoCmd {
                    workspace_id: workspace,
                    remote_url: url,
                    canonical_url: canonical,
                    tracked_branch: branch.clone(),
                    link_path,
                    link_branch: branch,
                    prefix,
                })
                .await?;
            render::attach_outcome(&outcome);
        }
        RepoCmd::Detach { id } => {
            let resolved = resolve_repo_handle_required(svc, &id).await?;
            svc.bindings.detach(&resolved).await?;
            println!("{}", serde_json::json!({ "detached": resolved }));
        }
        RepoCmd::List {
            ws: WorkspaceArg { workspace },
        } => render::repos(&svc.bindings.list(&workspace).await?),
        RepoCmd::Show { id } => match svc.bindings.show(&id).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Rename { repo, name } => match svc.bindings.rename(&repo, name).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::SetPrefix { repo, prefix } => match svc.bindings.set_prefix(&repo, prefix).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Alias(RepoAliasCmd::Add {
            repo,
            a: AliasArg { alias },
        }) => match svc.bindings.add_alias(&repo, alias).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Alias(RepoAliasCmd::Rm {
            repo,
            a: AliasArg { alias },
        }) => match svc.bindings.remove_alias(&repo, &alias).await {
            Ok(dto) => render::repo(&dto),
            Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                handle_ambiguous(query, candidates);
            }
            Err(e) => return Err(anyhow!("{e}")),
        },
        RepoCmd::Find { query } => render::find(&svc.bindings.find(&query).await?),
        RepoCmd::Doctor {
            ws: WorkspaceArg { workspace },
            repair,
            target,
        } => {
            // Resolve `--target` via the same handle resolver the rest of `rl
            // repo` uses (ambiguous matches exit 2 with the candidate list; an
            // unresolvable handle is a hard error — the user must know the new
            // home). RFC 0005 §D4: doctor re-points the filing axis, which lives
            // in ORIGIN id space, so the target is the binding's origin id — NOT
            // the per-workspace instance id the handle resolves to (passing the
            // instance id fails doctor's get_origin validation for any repo
            // where instance.id != origin.id).
            let target_uuid = match target {
                Some(handle) => {
                    let instance_id = resolve_repo_handle_required(svc, &handle).await?;
                    let binding = svc.bindings.show(&instance_id).await?;
                    Some(
                        binding
                            .origin_id
                            .parse::<domain_core::RepoId>()
                            .map_err(|e| {
                                anyhow!("invalid --target origin id {:?}: {e}", binding.origin_id)
                            })?,
                    )
                }
                None => None,
            };
            let summary = svc.bindings.doctor(&workspace, repair, target_uuid).await?;
            // Serialize failure on a known-good in-memory struct is
            // a programmer error (DoctorSummary's Serialize impl is
            // derived, all fields are valid). Don't paper over it
            // with a fabricated "0 results" payload that would
            // falsely tell the user the workspace is clean — the
            // doctor *did* find affected tasks, the JSON printer
            // is just broken. Propagate so the failure is loud.
            let out = serde_json::to_string_pretty(&summary)
                .map_err(|e| anyhow!("failed to render repo doctor summary: {e}"))?;
            println!("{out}");
        }
        RepoCmd::Discover { path } => {
            let mut rows = Vec::new();
            for repo_path in discover_repos_under(&path) {
                let canonical = discover_canonical(&repo_path).ok().flatten();
                rows.push(DiscoveredRepo {
                    path: repo_path.display().to_string(),
                    canonical,
                });
            }
            render::discovered(&rows);
        }
        RepoCmd::Locate {
            path,
            include_archived,
        } => {
            let candidate = match path {
                Some(p) => p,
                None => std::env::current_dir()
                    .map_err(|e| anyhow!("failed to determine current directory: {e}"))?,
            };
            let abs = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
            let query_path = abs.display().to_string();

            // Only "not a git repo" (or "git repo with no origin") maps to
            // null — those are legitimate no-matches. Any other error (git
            // binary missing, I/O failure, permission denied) is a real
            // problem worth surfacing so callers can distinguish broken
            // tooling from an unmapped path.
            let canonical_url = match discover_canonical(&abs) {
                Err(infra_git::GitError::NotARepo(_)) | Ok(None) => None,
                Err(e) => return Err(anyhow!("{e}")),
                Ok(Some(c)) => Some(c),
            };

            let matches = match canonical_url.as_deref() {
                Some(c) => {
                    svc.bindings
                        .memberships_for_canonical_url(c, include_archived)
                        .await?
                }
                None => vec![],
            };

            render::locate(&LocateResponseDto {
                query_path,
                canonical_url,
                matches,
            });
        }
    }
    Ok(())
}

/// Resolve the path that `repo attach` should register as a worktree.
///
/// Returns `Ok(None)` when the caller opted out via `--no-link`.
/// Otherwise discovers the cwd (or the explicit `--path`), verifies its
/// git origin canonicalises to `expected_canonical`, and returns the
/// absolute path string. All failure modes bail with a CLI-friendly
/// message that names the available escape hatches.
fn resolve_attach_link_path(
    path: Option<&std::path::Path>,
    no_link: bool,
    expected_canonical: &str,
) -> Result<Option<String>> {
    if no_link {
        return Ok(None);
    }

    let explicit_path = path.is_some();
    let candidate = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| anyhow!("failed to determine current directory: {e}"))?,
    };
    let abs = std::fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());

    match discover_canonical(&abs) {
        Err(infra_git::GitError::NotARepo(_)) if explicit_path => anyhow::bail!(
            "path is not a git repo: {}; pass a different --path or --no-link",
            abs.display()
        ),
        Err(infra_git::GitError::NotARepo(_)) => anyhow::bail!(
            "cwd is not a git repo: {}; pass --path <p> or --no-link",
            abs.display()
        ),
        Err(e) => Err(anyhow!("{e}")),
        Ok(None) => anyhow::bail!(
            "git repo at {} has no `origin` remote; pass --path <p> or --no-link",
            abs.display()
        ),
        Ok(Some(discovered)) if discovered != expected_canonical => anyhow::bail!(
            "path origin canonicalises to '{discovered}', not '{expected_canonical}'; pass --path or --no-link"
        ),
        Ok(Some(_)) => Ok(Some(abs.display().to_string())),
    }
}

/// Best-effort canonical form of `input` for looking up a stored worktree.
///
/// If `canonicalize` succeeds outright, use it. Otherwise walk up the path
/// to the longest *existing* prefix, canonicalise that (so any symlinked
/// component gets resolved), and rejoin the missing tail components. This
/// makes `unlink` match `link`-stored entries even after the target leaf
/// has been deleted, including the macOS `/var → /private/var` case.
///
/// Last-resort fallback: convert to absolute via cwd for relative inputs,
/// or pass the raw string through if even that fails.
fn canonicalize_for_lookup(input: &str) -> String {
    let raw = PathBuf::from(input);

    if let Ok(p) = std::fs::canonicalize(&raw) {
        return p.display().to_string();
    }

    // Pop components until we find a prefix that canonicalises. The popped
    // pieces get rejoined to that resolved prefix to reconstruct the full
    // intended path.
    let mut prefix = raw.clone();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while let Some(name) = prefix.file_name().map(|n| n.to_owned()) {
        if !prefix.pop() || prefix.as_os_str().is_empty() {
            break;
        }
        suffix.push(name);
        if let Ok(canonical) = std::fs::canonicalize(&prefix) {
            let mut result = canonical;
            for piece in suffix.iter().rev() {
                result.push(piece);
            }
            return result.display().to_string();
        }
    }

    // Nothing in the path existed. For relative inputs, anchor to cwd so
    // we at least produce an absolute string the service can compare.
    if raw.is_absolute() {
        raw.display().to_string()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&raw).display().to_string())
            .unwrap_or_else(|_| input.to_string())
    }
}

#[derive(serde::Serialize)]
pub struct DiscoveredRepo {
    pub path: String,
    pub canonical: Option<String>,
}

pub(crate) async fn worktree_dispatch(cmd: WorktreeCmd, svc: &Services) -> Result<()> {
    match cmd {
        WorktreeCmd::Link {
            repo,
            path,
            br: BranchArg { branch },
        } => {
            let raw_path = std::path::Path::new(&path);
            let abs_path =
                std::fs::canonicalize(raw_path).unwrap_or_else(|_| raw_path.to_path_buf());

            let discovered = match discover_canonical(&abs_path) {
                Err(infra_git::GitError::NotARepo(_)) => {
                    anyhow::bail!("path is not a git repo: {}", abs_path.display());
                }
                Err(e) => return Err(anyhow!("{e}")),
                Ok(None) => {
                    anyhow::bail!("git repo at {} has no `origin` remote", abs_path.display());
                }
                Ok(Some(c)) => c,
            };

            // Route through the same resolver as `rl repo show`: a prefix /
            // name / alias works wherever a UUID does. Ambiguous handles exit
            // 2 with the candidate JSON rather than collapsing into a generic
            // error from the `?`.
            let binding = match svc.bindings.show(&repo).await {
                Ok(b) => b,
                Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
                    handle_ambiguous(query, candidates)
                }
                Err(e) => return Err(anyhow!("{e}")),
            };
            if discovered != binding.canonical_url {
                // Surface every binding that matches the discovered canonical so
                // the user can pick the right `--repo`. Picking arbitrarily (e.g.
                // `.first()`) misleads when the canonical is bound in multiple
                // workspaces.
                let memberships = svc
                    .bindings
                    .memberships_for_canonical_url(&discovered, false)
                    .await?;
                let repo_short = &repo;
                match memberships.as_slice() {
                    [] => anyhow::bail!(
                        "path origin '{discovered}' doesn't match repo {repo_short} \
                         ('{}') and no binding matches '{discovered}'; \
                         run `rl repo attach` first",
                        binding.canonical_url
                    ),
                    [only] => anyhow::bail!(
                        "path origin '{discovered}' doesn't match repo {repo_short} \
                         ('{}'); use --repo {} instead",
                        binding.canonical_url,
                        only.binding.id
                    ),
                    many => {
                        let candidates = many
                            .iter()
                            .map(|m| format!("{} (workspace: {})", m.binding.id, m.workspace.name))
                            .collect::<Vec<_>>()
                            .join(", ");
                        anyhow::bail!(
                            "path origin '{discovered}' doesn't match repo {repo_short} \
                             ('{}'); canonical '{discovered}' is bound in multiple workspaces: \
                             {candidates}; choose --repo explicitly",
                            binding.canonical_url
                        );
                    }
                }
            }

            let dto = svc
                .bindings
                .link_worktree(LinkWorktreeCmd {
                    repo_id: binding.id,
                    path: abs_path.display().to_string(),
                    branch,
                })
                .await?;
            render::repo(&dto);
        }
        WorktreeCmd::Unlink { repo, path } => {
            let resolved = resolve_repo_handle_required(svc, &repo).await?;
            // Mirror link's canonicalisation so identical --path input
            // round-trips. When the leaf is gone we still try to resolve
            // any symlinked prefix so e.g. macOS `/var/...` matches the
            // stored `/private/var/...`.
            let canonical_path = canonicalize_for_lookup(&path);
            let dto = svc
                .bindings
                .unlink_worktree(UnlinkWorktreeCmd {
                    repo_id: resolved,
                    path: canonical_path,
                })
                .await?;
            render::repo(&dto);
        }
        WorktreeCmd::PruneMissing { repo } => {
            let resolved = resolve_repo_handle_required(svc, &repo).await?;
            let dto = svc.bindings.prune_missing(&resolved).await?;
            render::repo(&dto);
        }
        WorktreeCmd::Reconcile {
            ws: WorkspaceArg { workspace },
            prune,
        } => {
            let probe = TokioFilesystemProbe::new();
            let summary = svc
                .bindings
                .reconcile_worktrees(&workspace, &probe, prune)
                .await?;
            render::reconcile(&summary);
        }
    }
    Ok(())
}

/// Resolve a `--repo` argument (UUID / prefix / name / alias) to a binding
/// UUID, reusing the same resolver as `rl repo show`. `None` stays `None`.
/// Keeps `task create`/`edit` consistent with every other repo-addressing
/// command instead of demanding a raw UUID.
pub(crate) async fn resolve_repo_handle(
    svc: &Services,
    repo: Option<String>,
) -> Result<Option<String>> {
    match repo {
        Some(handle) => resolve_repo_handle_required(svc, &handle).await.map(Some),
        None => Ok(None),
    }
}

/// Required-arg sibling of `resolve_repo_handle`. Every command that takes a
/// repo positionally or via a non-optional `--repo` resolves through here so
/// a prefix / name / alias works in the same places a UUID does. Ambiguous
/// matches exit 2 with the same candidate JSON as `rl repo show`.
pub(crate) async fn resolve_repo_handle_required(svc: &Services, handle: &str) -> Result<String> {
    match svc.bindings.show(handle).await {
        Ok(dto) => Ok(dto.id),
        Err(application_workspace::ServiceError::AmbiguousHandle { query, candidates }) => {
            handle_ambiguous(query, candidates)
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}
