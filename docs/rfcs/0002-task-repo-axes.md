# RFC 0002 — Task repo axes: logical repo vs. filing repo

Status: Draft
Tracking epic: **#113**
Supersedes: RFC 0001 §D1 "Configurability deferred" (the `creation_default_repo_id` note)
Prerequisite: **#112** — the logical/filing terminology pass — must land **before any implementation work in this RFC begins.** Its goal is to kill the ambiguity that appears the moment a *filing* repo exists: a bare `repo` / `canonical` variable no longer says *which* repo it means. #112 adopts the logical/filing vocabulary in doc-comments, clap help, and internal locals/params — renaming a bare `repo`/`canonical` to `logical_*` / `filing_*` only where it would otherwise be ambiguous. `repo_id` itself stays (it is the logical repo, correctly named); no DB, DTO/JSON, or CLI-flag renames. All behaviour-preserving. Doing #112 first keeps this RFC's structural diff small. See §5.

## 1. Context

`Task.repo_id` conflates two distinct facts:

1. **Logical repo** — where the code/work lives, where the agent's worktrees are on disk, and the source of the task's friendly-ID prefix.
2. **Filing repo** — the repo whose `canonical_url` the GitHub issue is actually created in.

Today these are the same field, so the issue always lands in the logical repo. RFC 0001 §D1 shipped this conflated model deliberately (and deferred `creation_default_repo_id` as a "later, cheap" override), but it cannot express topologies where the two diverge.

### Terminology

This RFC leans on two terms, used consistently throughout:

- **Logical repo** (`Task.repo_id`) — the repo the work belongs to: where the code lives, where worktrees are checked out, and the source of the task's friendly-ID prefix. This is the existing field; its meaning is unchanged.
- **Filing repo** (`*.filing_repo_id`) — where the backing GitHub issue is created and lives. New. May differ from the logical repo, or be absent (a project board draft with no repo at all).

A note on naming: the logical repo is the repo the work *canonically* belongs to, so "canonical repo" reads naturally — but `canonical_url` already names the normalized git remote on a binding (`RepoBinding.canonical_url`). To avoid overloading "canonical," this RFC says **logical repo** everywhere. The filing repo is never called "sync target" or "issue repo" — one term, **filing repo**.

### Motivating topology

A team (`team-eng`) owns several services:

- Repos **A**, **B**, **C** — individual services. Code lives here; this is where engineers (and agents) check out and work. These are **logical repos**.
- Repo **`team-eng`** — a dedicated *issues* repo. By consensus the team files **all** their GitHub issues here rather than in A/B/C. This is the **filing repo**.
- A **Projects v2 board** that tracks those tickets. It *can* hold issues from A/B/C, but the consensus is they come from `team-eng`.

The desired behaviour: working in repo A, `rl task create "fix booking bug"` should produce a task whose **logical repo is A** (prefix, worktree/on-disk context) but whose **issue is filed in `team-eng`** and **added to the board**.

The current model can't say this:

- `repo_id = A` → issue filed in A (wrong repo).
- `repo_id = team-eng` → issue lands correctly, but the task *becomes* a team-eng task: it takes team-eng's prefix and loses its association with service A's checkout.

Two distinct facts, one field.

## 2. Decisions

### D1 — Two repo axes

Keep `Task.repo_id` as the **logical repo** (worktrees, prefix, where the agent works). Introduce a separate **filing repo** axis: where the backing GitHub issue is created. This is the split flagged when RFC 0001 was scoped but not implemented.

### D2 — The filing repo is a workspace default, recorded per task on promote

The team's consensus is workspace/project-wide ("file in `team-eng`"), so the filing repo's natural home is a **workspace-level default**, not a per-task decision. Resolution precedence at create/promote:

1. explicit per-task override (the `--filing-repo` flag), else
2. the workspace's filing-repo default, else
3. the logical `repo_id` (**today's behaviour** — issue lands in the logical repo), else
4. project workspace with no repo at all → board draft (RFC 0001 path 1, unchanged).

The **resolved** filing repo is recorded on the task at promote time (stable, like `project_item_id`) so that later changes to the workspace default never silently move an already-filed issue.

This keeps RFC 0001 behaviour intact: with no workspace default and no override, the filing repo resolves to `repo_id` exactly as today. The new axis only diverges when a workspace opts into a dedicated issues repo. The split is therefore **additive**, not an inversion of `repo_id`'s meaning.

### D3 — Interaction with the project axis (RFC 0001)

The **project** remains the primary sync target for *board membership*; the **filing repo** decides only *where the backing issue lives*. RFC 0001's orphan-draft and `convertProjectV2DraftIssueItemToIssue` paths still apply — except "convert to issue" now targets the resolved **filing repo**, not the logical repo.

### D4 — Identity stays logical

Friendly task IDs keep using the **logical** repo's prefix, so a task about service A reads as `a-xxx` even though its issue lives in `team-eng`. The filing repo is a sync detail, not an identity. (Open question §6 — confirm this is the desired reading.)

### D5 — The filing repo is internal, not part of the task DTO

The filing repo is a sync/persistence detail, not part of a task's public shape. `Task.repo_id` (logical) remains the only repo on the task DTO/JSON; `filing_repo_id` lives in the DB and on the domain `Task`, consumed internally by the promote/sync path. The only public surface for *setting* it is the additive `--filing-repo` CLI flag. Consequence: **zero consumer-contract churn** — every existing `repo_id` reader (the rl-tasks skill, `jq '.repo_id'`) keeps working untouched, and the split stays fully additive at the boundary.

## 3. Schema sketch

- `workspaces` gains `filing_repo_id` (nullable FK → repo binding) — the workspace's **default** filing repo. This **repurposes / replaces** RFC 0001's deferred `creation_default_repo_id`.
- `tasks` gains `filing_repo_id` (nullable) — the **resolved** filing repo, set on promote. `repo_id` is unchanged (logical).
- Migration is additive. Existing rows get `filing_repo_id = NULL`, which resolves to `repo_id` — preserving current behaviour with no data movement.

Both columns share the name `filing_repo_id`: same concept (where the issue is filed), differing only in scope — the workspace row is the *default*, the task row is the *resolved-and-recorded* value.

`tasks.filing_repo_id` is **internal** — persisted and carried on the domain `Task`, but **not surfaced on the task DTO/JSON** (see D5). The task DTO continues to expose only `repo_id` (logical).

## 4. CLI surface

- Set the workspace filing repo — either a verb (`rl workspace set-filing-repo <repo>`) or, preferably, declared in `repo-link.toml` (RFC 0001 §9 / issue #91).
- `rl task create --repo <logical> [--filing-repo <filing>]`.
- `rl task show` surfaces both axes so the divergence is legible.

## 5. Relationship to existing work

- **#112** (logical/filing terminology pass) — the **prerequisite**: adopt the "logical repo" / "filing repo" vocabulary in doc-comments, clap help, module docs, and internal locals/params. **Disambiguation-driven** — rename a bare `repo`/`canonical` to `logical_*`/`filing_*` only where a reader couldn't otherwise tell which repo is meant once filing exists. `repo_id` stays; no DB, DTO/JSON, or CLI-flag renames (the filing repo never reaches those surfaces — see D5). Do this first so the implementation diff here is purely the structural split.
- **Supersedes** RFC 0001 §D1's deferred `creation_default_repo_id`.
- **#90** (`task create`: infer repo binding from cwd) feeds the **logical** repo — a precedence layer for `repo_id`, orthogonal to the filing axis.
- **#91** (TOML per-repo/workspace config + AGENTS.md roster) is the natural place to declare the workspace filing repo and project attachment. #91 is **BlockedBy** this RFC's epic (**#113**) — its "default filing repo" only has meaning once this axis exists.
- **#71** (cross-repo transfer via `transferIssue`) becomes "change the filing repo on an already-synced task." The synced case waits on this split; the local-only case already works via `rl task edit --repo`.

## 6. Out of scope / open questions

- **Per-task override UX** — exact shape of `--filing-repo` and whether it accepts the same handle forms (prefix/name/alias/UUID) as `--repo`.
- **Filing repo not checked out locally** — `team-eng` may have no worktree on disk. Filing needs only its `canonical_url`, not a checkout, so this is fine; confirm `repo attach` can register a binding without a worktree.
- **Multiple filing repos per workspace** — out of scope; one default + per-task override covers the exceptions.
- **Remote-identity scope** — the existing remote-task uniqueness key is `(repo_id, provider, remote_id)`, which assumes the *logical* repo scopes a remote issue's identity. Once filing ≠ logical, the issue actually lives in the **filing** repo, so this key should almost certainly become `(filing_repo_id, provider, remote_id)`. Confirm and migrate during implementation (touches the `tasks` unique index and `TaskRepository::get_remote`).
- **D4 prefix question** — confirm logical-repo prefix is the desired identity for cross-filed tasks.
- **Synced transfer** (`transferIssue`) — defer to #71 once this lands.
