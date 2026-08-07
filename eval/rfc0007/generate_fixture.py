#!/usr/bin/env python3
"""RFC 0007 §10 labelled retrieval fixture. Synthetic task content, queries,
and labelled relevant task IDs meeting the predeclared per-category minimums.

Run: python3 generate_fixture.py  ->  fixture.json + fixture.md
"""

from __future__ import annotations

import json
from pathlib import Path

OUT = Path(__file__).parent / "fixture.json"
MD = Path(__file__).parent / "fixture.md"

TASKS: list[dict] = [
    {
        "id": "t-fast-create",
        "title": "Create a task quickly from the command line",
        "body": (
            "Users want to file a task without opening the full editor.\n"
            "The CLI should accept a one-line title and optional body on the "
            "command line and create a local task immediately.\n"
            "No remote sync is attempted until the user runs the sync command."
        ),
        "comments": ["Nice, this unblocks scripting."],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-instant-issue",
        "title": "One-line issue filing from the terminal",
        "body": (
            "Filing a ticket should not require a GUI.\n"
            "A single command with a title argument is enough to record the "
            "work item locally; the body stays optional.\n"
            "Later promotion to a remote tracker happens explicitly."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-bulk-import",
        "title": "Bulk import of existing tasks from a CSV",
        "body": (
            "Migration from a spreadsheet should map rows to tasks.\n"
            "Columns for title, description, and status are recognised; "
            "unknown columns are ignored with a warning.\n"
            "Duplicates by title are skipped."
        ),
        "comments": ["Watch out for quoted commas in the CSV."],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-sync-direction",
        "title": "Clarify the direction of the sync operation",
        "body": (
            "The sync verb is ambiguous: does it push local changes to the "
            "remote or pull remote changes down?\n"
            "Default should be a two-way merge with local winning on conflict, "
            "but the flag names must make the axis explicit."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-push-pull",
        "title": "Push and pull should be explicit verbs",
        "body": (
            "Rather than one magical sync, expose separate push and pull "
            "operations so the user controls the direction.\n"
            "A combined sync remains available for the common two-way case."
        ),
        "comments": ["Agreed — explicit beats implicit here."],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-conflict-reconcile",
        "title": "Reconcile conflicting edits between local and remote",
        "body": (
            "When a task changes on both sides, decide which version wins.\n"
            "Local-first projects want local edits to win; team-first projects "
            "may prefer the remote revision.\n"
            "The policy must be configurable and the loser must not be "
            "silently discarded."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-merge-rule",
        "title": "Configurable merge conflict policy",
        "body": (
            "Give users a setting that controls who wins a conflict.\n"
            "Options: local always, remote always, or keep both as separate "
            "tasks.\n"
            "Default local; the non-winning version is retained in history."
        ),
        "comments": [
            "Bikeshed alert — make local the default and move on.",
        ],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-task-code",
        "title": "Tasks should expose a stable short identifier",
        "body": (
            "Every task carries a short human-readable code such as "
            "'wfl-abc'.\n"
            "The code is stable across renames (unlike the record id) and is "
            "the canonical handle in shell output."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-short-handle",
        "title": "Short handle for referencing a task in scripts",
        "body": (
            "Scripts need a short, stable token to refer to a task.\n"
            "A two-part code in the form word-letters is easy to type in a "
            "shell and survives title changes."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-error-fmt",
        "title": "Error: E_TASK_NOT_FOUND shown when a task id is missing",
        "body": (
            "Looking up a task by a code that does not exist prints "
            "'error: task not found (E_TASK_NOT_FOUND)'.\n"
            "The exit code should be non-zero so scripts can detect it."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-error-code",
        "title": "Distinct error codes for sync failures",
        "body": (
            "Every user-facing failure mode gets an enum-style code:\n"
            "E_TASK_NOT_FOUND, E_CONFLICT, E_NETWORK, E_AUTH.\n"
            "Codes are stable contract, never free-form messages."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-lockfile",
        "title": "Use a lock file to avoid concurrent writers",
        "body": (
            "Two concurrent commands that both write must not corrupt the "
            "store.\n"
            "A standard file lock around the database connection is enough; "
            "a full daemon is overkill."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-file-lock",
        "title": "Lock the database file during writes",
        "body": (
            "Concurrent editors must not clobber each other's writes.\n"
            "A shared/exclusive lock on the backing file serialises writers "
            "without a separate server process."
        ),
        "comments": [],
        "status": "closed",
        "language": "en",
    },
    {
        "id": "t-semantic-search",
        "title": "Find a task from a concept, not the exact words",
        "body": (
            "Users should be able to retrieve a task by describing what it "
            "does rather than quoting its title.\n"
            "A dense embedding over task text closes the wording gap while "
            "exact and lexical search keep precision."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-search-by-meaning",
        "title": "Look up work by what it accomplishes",
        "body": (
            "Instead of matching words, match the intent behind a task.\n"
            "Vector similarity makes 'audit reasons for failure' find a task "
            "about logging retry causes even when they share few words."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-exact-match",
        "title": "Exact matching for error strings and identifiers",
        "body": (
            "When a user pastes an identifier or error message, only exact "
            "substring hits are useful.\n"
            "This lane scans raw text so a chunk boundary can never hide a "
            "match."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-due-reminder",
        "title": "Remind about tasks approaching their deadline",
        "body": (
            "Surface tasks whose due date is within 48 hours.\n"
            "The digest at session start should list them first, before "
            "merely-recent items."
        ),
        "comments": ["Prioritise by due date, then recency."],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-deadline-alert",
        "title": "Alert me before a task is overdue",
        "body": (
            "Warn when a task's deadline is near so nothing slips.\n"
            "The morning digest puts imminent deadlines ahead of recent but "
            "low-urgency items."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-multi-repo",
        "title": "Group tasks by the repository they belong to",
        "body": (
            "A workspace can span several repositories.\n"
            "Task queries should be able to scope to one repository by its "
            "handle."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-scope-repo",
        "title": "Filter work to a single codebase",
        "body": (
            "When a team owns several projects, narrowing queries to one "
            "repo keeps results focused.\n"
            "A --repo flag on list and search does this."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-old-vouchers",
        "title": "Retire the legacy gift voucher codes",
        "body": (
            "The old single-use voucher format is replaced by package "
            "gifting.\n"
            "New codes must not collide with the abandoned voucher prefix."
        ),
        "comments": [],
        "status": "closed",
        "language": "en",
    },
    {
        "id": "t-import-sprint",
        "title": "Import completed sprint items from the tracking sheet",
        "body": (
            "Closed sprint rows came in from a spreadsheet so they remain "
            "searchable.\n"
            "Even done, they should surface when someone looks up that work."
        ),
        "comments": [],
        "status": "closed",
        "language": "en",
    },
    {
        "id": "t-migrate-config",
        "title": "Migrate the old config file to the new format",
        "body": (
            "The previous config schema is deprecated.\n"
            "A one-time migration rewrites it in place with a backup copy."
        ),
        "comments": [],
        "status": "closed",
        "language": "en",
    },
    {
        "id": "t-auth-refresh",
        "title": "Refresh the remote token automatically",
        "body": (
            "The API token expires; refresh it before it lapses.\n"
            "Store the new token with the same restricted file mode."
        ),
        "comments": [],
        "status": "closed",
        "language": "en",
    },
    {
        "id": "t-batch-sync",
        "title": "Batch-sync many tasks in one pass",
        "body": (
            "Overview: sync performance work.\n"
            "Detail: when the backlog is large, syncing task by task is slow.\n"
            "The real decision: queue all pending writes and flush them in one "
            "transaction batch to cut round-trips."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-throttle",
        "title": "Avoid hitting the remote API rate limit",
        "body": (
            "This ticket is about networking robustness.\n"
            "Some related background: the remote API allows a fixed number of "
            "requests per minute.\n"
            "The actual requirement: back off with exponential jitter when the "
            "remote reports we are over the limit."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-batch-retry",
        "title": "Group background retries into one request burst",
        "body": (
            "Context and motivation for this improvement.\n"
            "A note on observing failures: each retry currently issues a "
            "separate call.\n"
            "The decision that matters: collect failed items and retry them "
            "together in a single batched call to reduce load."
        ),
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-focus-search",
        "title": "Keep search results focused on the relevant task",
        "body": (
            "Search should rank a task by its core content, not its comment "
            "volume."
        ),
        "comments": [
            "hello", "test", "test", "lgtm", "bump", "ping",
            "is anyone here", "noise", "also test", "ty",
        ],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-concise-result",
        "title": "Concise result summaries in search output",
        "body": (
            "A long discussion thread should not dominate a task's rank "
            "against a short, on-topic description."
        ),
        "comments": [
            "ok", "ok", "done", "thanks", "next", "sure", "noted",
            "great", "fine", "yep",
        ],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-es-tarea",
        "title": "Crear una tarea desde la línea de comandos",
        "body": (
            "Los usuarios quieren registrar un trabajo sin abrir el editor.\n"
            "El comando acepta un título y un cuerpo opcional."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-sincronizar",
        "title": "Sincronizar cambios locales con el servidor remoto",
        "body": (
            "El comando de sincronización debe enviar los cambios locales y "
            "recibir los del servidor.\n"
            "En un conflicto, la versión local gana por defecto."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-buscar",
        "title": "Buscar una tarea por su significado",
        "body": (
            "El usuario debe poder encontrar una tarea describiendo qué hace, "
            "no citando su título exacto.\n"
            "La búsqueda semántica cierra la brecha entre palabras distintas."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-codigo",
        "title": "Exponer un identificador corto y estable",
        "body": (
            "Cada tarea muestra un código breve como 'wfl-abc'.\n"
            "El código no cambia al renombrar la tarea."
        ),
        "comments": [],
        "status": "closed",
        "language": "es",
    },
    {
        "id": "t-es-recordatorio",
        "title": "Recordar las tareas que vencen pronto",
        "body": (
            "Mostrar las tareas cuya fecha límite está a menos de 48 horas.\n"
            "El resumen de la mañana las lista primero."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-importar",
        "title": "Importar tareas en bloque desde una hoja de cálculo",
        "body": (
            "La migración desde una tabla debe mapear filas a tareas.\n"
            "Las columnas de título y estado se reconocen automáticamente."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-conflicto",
        "title": "Resolver ediciones en conflicto entre local y remoto",
        "body": (
            "Cuando una tarea cambia en ambos lados, hay que decidir qué "
            "versión gana.\n"
            "El proyecto local debe poder configurar la política."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-bloqueo",
        "title": "Bloquear la base de datos durante las escrituras",
        "body": (
            "Dos comandos concurrentes no deben corromper el almacén.\n"
            "Un bloqueo de archivo es suficiente."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-error",
        "title": "Códigos de error distintos para fallos de sincronización",
        "body": (
            "Cada modo de fallo tiene un código tipo enum.\n"
            "E_TASK_NOT_FOUND, E_CONFLICTO, E_RED.\n"
            "Los códigos forman parte del contrato."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-es-busqueda-exacta",
        "title": "Coincidencia exacta para identificadores y mensajes",
        "body": (
            "Al pegar un identificador, solo sirven las coincidencias exactas "
            "de subcadena.\n"
            "Este carril examina el texto original sin índices."
        ),
        "comments": [],
        "status": "open",
        "language": "es",
    },
    {
        "id": "t-theme-dark",
        "title": "Add a dark colour theme to the report view",
        "body": "Cosmetic change: a dark palette for the printed report view.",
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-font-size",
        "title": "Allow adjusting the font size",
        "body": "Make the text size configurable in the preference panel.",
        "comments": [],
        "status": "closed",
        "language": "en",
    },
    {
        "id": "t-keyboard-copy",
        "title": "Keyboard shortcut to copy a task link",
        "body": "Add a shortcut that copies the canonical link for a task.",
        "comments": [],
        "status": "open",
        "language": "en",
    },
    {
        "id": "t-export-json",
        "title": "Export the backlog as JSON",
        "body": "Provide an export command that writes all tasks as JSON.",
        "comments": [],
        "status": "closed",
        "language": "en",
    },
    {
        "id": "t-print-report",
        "title": "Print a weekly report of completed work",
        "body": "Generate a printable summary of what finished this week.",
        "comments": [],
        "status": "open",
        "language": "en",
    },
]

QUERIES: list[dict] = [
    {"category": "exact", "text": "wfl-abc", "relevant": ["t-task-code", "t-short-handle"]},
    {"category": "exact", "text": "E_TASK_NOT_FOUND", "relevant": ["t-error-fmt", "t-error-code"]},
    {"category": "exact", "text": "error: task not found", "relevant": ["t-error-fmt"]},
    {"category": "exact", "text": "E_CONFLICT", "relevant": ["t-error-code"]},
    {"category": "exact", "text": "E_NETWORK", "relevant": ["t-error-code"]},
    {"category": "exact", "text": "E_AUTH", "relevant": ["t-error-code"]},
    {"category": "exact", "text": "lock file", "relevant": ["t-lockfile", "t-file-lock"]},
    {"category": "exact", "text": "batch-sync", "relevant": ["t-batch-sync"]},
    {"category": "exact", "text": "CSV", "relevant": ["t-bulk-import"]},
    {"category": "exact", "text": "JSON export", "relevant": ["t-export-json"]},
    {"category": "exact", "text": "--repo", "relevant": ["t-scope-repo"]},
    {"category": "exact", "text": "48 hours", "relevant": ["t-due-reminder"]},
    {"category": "exact", "text": "wfl-abc task code", "relevant": ["t-task-code", "t-short-handle"]},
    {"category": "exact", "text": "config file migration", "relevant": ["t-migrate-config"]},
    {"category": "exact", "text": "token refresh", "relevant": ["t-auth-refresh"]},
    {"category": "exact", "text": "expires", "relevant": ["t-auth-refresh"]},
    {"category": "exact", "text": "rate limit", "relevant": ["t-throttle"]},
    {"category": "exact", "text": "dark theme", "relevant": ["t-theme-dark"]},
    {"category": "exact", "text": "font size", "relevant": ["t-font-size"]},
    {"category": "exact", "text": "copy task link", "relevant": ["t-keyboard-copy"]},
    {"category": "exact", "text": "weekly report", "relevant": ["t-print-report"]},
    {"category": "paraphrase", "text": "jot down a todo without the editor", "relevant": ["t-fast-create", "t-instant-issue"]},
    {"category": "paraphrase", "text": "suck all the rows out of the spreadsheet", "relevant": ["t-bulk-import"]},
    {"category": "paraphrase", "text": "who wins when both sides changed a record", "relevant": ["t-conflict-reconcile", "t-merge-rule"]},
    {"category": "paraphrase", "text": "send local adjustments up to the tracking site", "relevant": ["t-push-pull", "t-sync-direction"]},
    {"category": "paraphrase", "text": "stop two editors tripping over each other", "relevant": ["t-lockfile", "t-file-lock"]},
    {"category": "paraphrase", "text": "recall a job by what it is for, not its name", "relevant": ["t-semantic-search", "t-search-by-meaning"]},
    {"category": "paraphrase", "text": "surface jobs about to slip their date", "relevant": ["t-due-reminder", "t-deadline-alert"]},
    {"category": "paraphrase", "text": "narrow things down to one project we own", "relevant": ["t-multi-repo", "t-scope-repo"]},
    {"category": "paraphrase", "text": "push several pending edits in a single swoop", "relevant": ["t-batch-sync", "t-batch-retry"]},
    {"category": "paraphrase", "text": "don't get throttled by the upstream service", "relevant": ["t-throttle"]},
    {"category": "paraphrase", "text": "make a short key to point at a ticket in scripts", "relevant": ["t-short-handle", "t-task-code"]},
    {"category": "paraphrase", "text": "give every failure a consistent sign to catch", "relevant": ["t-error-code"]},
    {"category": "paraphrase", "text": "keep chatty discussion from inflating rank", "relevant": ["t-focus-search", "t-concise-result"]},
    {"category": "paraphrase", "text": "find a past change by what it accomplished", "relevant": ["t-import-sprint", "t-old-vouchers"]},
    {"category": "paraphrase", "text": "carry over an expired credential automatically", "relevant": ["t-auth-refresh"]},
    {"category": "paraphrase", "text": "translate a legacy settings file for the new release", "relevant": ["t-migrate-config"]},
    {"category": "paraphrase", "text": "group work by the code it lives in", "relevant": ["t-multi-repo"]},
    {"category": "paraphrase", "text": "check something urgent beats something old at logon", "relevant": ["t-due-reminder", "t-deadline-alert"]},
    {"category": "paraphrase", "text": "recall a ticket even though you forgot its wording", "relevant": ["t-search-by-meaning"]},
    {"category": "paraphrase", "text": "make writing items down scriptable from the shell", "relevant": ["t-fast-create"]},
    {"category": "paraphrase", "text": "keep old finished records findable later", "relevant": ["t-import-sprint", "t-old-vouchers"]},
    {"category": "paraphrase", "text": "prevent a server ban by freezing requests with noise", "relevant": ["t-throttle"]},
    {"category": "paraphrase", "text": "a recurring bump reminder near the clock", "relevant": ["t-deadline-alert"]},
    {"category": "paraphrase", "text": "put the exact pasted phrase first, ignore synonyms", "relevant": ["t-exact-match"]},
    {"category": "paraphrase", "text": "decide between my copy and their copy on both sides", "relevant": ["t-conflict-reconcile", "t-merge-rule"]},
    {"category": "paraphrase", "text": "scrape prior rows from a tabular sheet into the tracker", "relevant": ["t-bulk-import"]},
    {"category": "paraphrase", "text": "whose version is kept when divergent, then store the loser", "relevant": ["t-merge-rule"]},
    {"category": "paraphrase", "text": "avoid a separate daemon just to guard a single file", "relevant": ["t-lockfile", "t-file-lock"]},
    {"category": "paraphrase", "text": "understand a task's intent rather than matching letters", "relevant": ["t-semantic-search", "t-search-by-meaning"]},
    {"category": "paraphrase", "text": "handle a hundred records at once without a grind", "relevant": ["t-batch-sync"]},
    {"category": "misleading", "text": "search", "relevant": ["t-semantic-search", "t-search-by-meaning", "t-exact-match", "t-focus-search"]},
    {"category": "misleading", "text": "sync", "relevant": ["t-sync-direction", "t-push-pull", "t-conflict-reconcile", "t-batch-sync"]},
    {"category": "misleading", "text": "task", "relevant": ["t-fast-create", "t-instant-issue", "t-bulk-import", "t-task-code"]},
    {"category": "misleading", "text": "code", "relevant": ["t-task-code", "t-short-handle", "t-error-code"]},
    {"category": "misleading", "text": "error", "relevant": ["t-error-fmt", "t-error-code"]},
    {"category": "misleading", "text": "create", "relevant": ["t-fast-create", "t-instant-issue"]},
    {"category": "misleading", "text": "remote", "relevant": ["t-sync-direction", "t-push-pull", "t-auth-refresh", "t-throttle"]},
    {"category": "misleading", "text": "retry", "relevant": ["t-batch-retry", "t-throttle"]},
    {"category": "misleading", "text": "date", "relevant": ["t-due-reminder", "t-deadline-alert"]},
    {"category": "misleading", "text": "limit", "relevant": ["t-throttle"]},
    {"category": "misleading", "text": "file", "relevant": ["t-lockfile", "t-file-lock", "t-migrate-config"]},
    {"category": "misleading", "text": "important", "relevant": ["t-due-reminder", "t-deadline-alert"]},
    {"category": "misleading", "text": "rank", "relevant": ["t-focus-search", "t-concise-result", "t-semantic-search"]},
    {"category": "misleading", "text": "review", "relevant": ["t-print-report"]},
    {"category": "misleading", "text": "export", "relevant": ["t-export-json", "t-print-report"]},
    {"category": "long_desc", "text": "flush pending writes in one batched transaction", "relevant": ["t-batch-sync"]},
    {"category": "long_desc", "text": "back off with jitter when over the remote limit", "relevant": ["t-throttle"]},
    {"category": "long_desc", "text": "retry failed items together in a single call", "relevant": ["t-batch-retry"]},
    {"category": "long_desc", "text": "queue pending writes and flush in one transaction batch", "relevant": ["t-batch-sync"]},
    {"category": "long_desc", "text": "the real decision is the batched flush to cut round-trips", "relevant": ["t-batch-sync"]},
    {"category": "long_desc", "text": "robustness work means backing off when the API caps us", "relevant": ["t-throttle"]},
    {"category": "long_desc", "text": "beyond the networking overview we need exponential jitter backoff", "relevant": ["t-throttle"]},
    {"category": "long_desc", "text": "fold the failed items into one retry burst rather than separate calls", "relevant": ["t-batch-retry"]},
    {"category": "long_desc", "text": "the load-reducing fix is collecting failures and retrying them together", "relevant": ["t-batch-retry"]},
    {"category": "long_desc", "text": "reduce round trips by flushing all pending writes in one batch", "relevant": ["t-batch-sync"]},
    {"category": "length_bias", "text": "rank by core content not comment volume", "relevant": ["t-focus-search", "t-concise-result"]},
    {"category": "length_bias", "text": "a noisy thread must not dominate the summary", "relevant": ["t-concise-result"]},
    {"category": "length_bias", "text": "comment noise should not push the rank of a focused task", "relevant": ["t-focus-search"]},
    {"category": "length_bias", "text": "volume of chatter must not outweigh the description", "relevant": ["t-concise-result"]},
    {"category": "length_bias", "text": "a first paragraph of empty replies must not defeat relevance", "relevant": ["t-focus-search"]},
    {"category": "length_bias", "text": "keep the thread length from skewing the result ordering", "relevant": ["t-focus-search", "t-concise-result"]},
    {"category": "length_bias", "text": "discussion length should not rank the whole result", "relevant": ["t-concise-result"]},
    {"category": "length_bias", "text": "irrelevant check-ins must not inflate a task's standing", "relevant": ["t-focus-search"]},
    {"category": "length_bias", "text": "core content should beat an echoing comment wall", "relevant": ["t-focus-search"]},
    {"category": "length_bias", "text": "a quiet on-topic description should outrank a loud off-topic thread", "relevant": ["t-concise-result"]},
    {"category": "typo", "text": "syncronise", "relevant": ["t-sync-direction", "t-push-pull"]},
    {"category": "typo", "text": "conflct", "relevant": ["t-conflict-reconcile", "t-merge-rule"]},
    {"category": "typo", "text": "sarch", "relevant": ["t-semantic-search", "t-search-by-meaning"]},
    {"category": "typo", "text": "deadline aler", "relevant": ["t-due-reminder", "t-deadline-alert"]},
    {"category": "typo", "text": "importe", "relevant": ["t-bulk-import", "t-import-sprint"]},
    {"category": "typo", "text": "throttel", "relevant": ["t-throttle"]},
    {"category": "typo", "text": "batch-syn", "relevant": ["t-batch-sync"]},
    {"category": "typo", "text": "voucher", "relevant": ["t-old-vouchers"]},
    {"category": "typo", "text": "conflict polcy", "relevant": ["t-merge-rule"]},
    {"category": "typo", "text": "semntic", "relevant": ["t-semantic-search", "t-search-by-meaning"]},
    {"category": "language", "language": "en", "text": "create a task from the command line", "relevant": ["t-fast-create", "t-instant-issue"]},
    {"category": "language", "language": "en", "text": "sync local and remote changes", "relevant": ["t-sync-direction", "t-push-pull"]},
    {"category": "language", "language": "en", "text": "resolve a conflict between local and remote", "relevant": ["t-conflict-reconcile", "t-merge-rule"]},
    {"category": "language", "language": "en", "text": "search for a task by meaning", "relevant": ["t-semantic-search", "t-search-by-meaning"]},
    {"category": "language", "language": "en", "text": "stable short identifier", "relevant": ["t-task-code", "t-short-handle"]},
    {"category": "language", "language": "en", "text": "remind about imminent deadlines", "relevant": ["t-due-reminder", "t-deadline-alert"]},
    {"category": "language", "language": "en", "text": "bulk import from spreadsheet", "relevant": ["t-bulk-import"]},
    {"category": "language", "language": "en", "text": "error codes for sync failures", "relevant": ["t-error-code"]},
    {"category": "language", "language": "en", "text": "lock the database during writes", "relevant": ["t-lockfile", "t-file-lock"]},
    {"category": "language", "language": "en", "text": "exact matching for error strings", "relevant": ["t-exact-match"]},
    {"category": "language", "language": "es", "text": "crear una tarea desde la línea de comandos", "relevant": ["t-es-tarea", "t-fast-create"]},
    {"category": "language", "language": "es", "text": "sincronizar cambios locales y remotos", "relevant": ["t-es-sincronizar", "t-sync-direction"]},
    {"category": "language", "language": "es", "text": "buscar una tarea por su significado", "relevant": ["t-es-buscar", "t-semantic-search"]},
    {"category": "language", "language": "es", "text": "identificador corto y estable", "relevant": ["t-es-codigo", "t-task-code"]},
    {"category": "language", "language": "es", "text": "recordar tareas que vencen pronto", "relevant": ["t-es-recordatorio", "t-due-reminder"]},
    {"category": "language", "language": "es", "text": "importar tareas en bloque", "relevant": ["t-es-importar", "t-bulk-import"]},
    {"category": "language", "language": "es", "text": "resolver un conflicto entre local y remoto", "relevant": ["t-es-conflicto", "t-conflict-reconcile"]},
    {"category": "language", "language": "es", "text": "bloquear la base de datos al escribir", "relevant": ["t-es-bloqueo", "t-lockfile"]},
    {"category": "language", "language": "es", "text": "códigos de error para fallos de sincronización", "relevant": ["t-es-error", "t-error-code"]},
    {"category": "language", "language": "es", "text": "coincidencia exacta para identificadores", "relevant": ["t-es-busqueda-exacta", "t-exact-match"]},
    {"category": "closed", "text": "voucher gift code retirement", "relevant": ["t-old-vouchers"]},
    {"category": "closed", "text": "import completed sprint items", "relevant": ["t-import-sprint"]},
    {"category": "closed", "text": "old config schema migration", "relevant": ["t-migrate-config"]},
    {"category": "closed", "text": "automatic token refresh", "relevant": ["t-auth-refresh"]},
    {"category": "closed", "text": "export backlog as JSON", "relevant": ["t-export-json"]},
    {"category": "closed", "text": "adjust the font size", "relevant": ["t-font-size"]},
    {"category": "closed", "text": "lock the database file during writes", "relevant": ["t-file-lock"]},
    {"category": "closed", "text": "short handle for scripts", "relevant": ["t-short-handle"]},
    {"category": "closed", "text": "dark colour theme", "relevant": ["t-theme-dark"]},
    {"category": "closed", "text": "migrate the legacy config file", "relevant": ["t-migrate-config"]},
]


def category_minimums() -> dict[str, int]:
    return {
        "exact": 20,
        "paraphrase": 30,
        "misleading": 15,
        "long_desc": 10,
        "length_bias": 10,
        "typo": 10,
        "language": 10,
        "closed": 10,
    }


def count_by_category() -> dict[str, dict[str, int]]:
    counts: dict[str, dict[str, int]] = {}
    for q in QUERIES:
        counts.setdefault(q["category"], {}).setdefault(
            q.get("language", "default"), 0
        )
        counts[q["category"]][q.get("language", "default")] += 1
    return counts


def check_minimums() -> list[str]:
    problems: list[str] = []
    mins = category_minimums()
    counts = count_by_category()
    for cat, min_count in mins.items():
        if cat == "language":
            for lang, n in counts.get("language", {}).items():
                if n < min_count:
                    problems.append(
                        f"language[{lang}] has {n}, need >= {min_count}"
                    )
            continue
        n = sum(counts.get(cat, {}).values())
        if n < min_count:
            problems.append(f"{cat} has {n}, need >= {min_count}")
    return problems


def render_markdown() -> str:
    lines = [
        "# RFC 0007 — Stage 2 labelled retrieval fixture",
        "",
        "Synthetic, sanitized fixture (RFC 0007 §10): task content, queries, and",
        "labelled relevant task IDs. No real task content is used.",
        "",
        "Regenerate deterministically with `python3 generate_fixture.py`.",
        "",
    ]
    counts = count_by_category()
    lines.append("## Category counts")
    lines.append("")
    lines.append("| Category | Count | Minimum |")
    lines.append("|---|---|---|")
    for cat, minc in category_minimums().items():
        if cat == "language":
            for lang, n in sorted(counts.get("language", {}).items()):
                lines.append(f"| language ({lang}) | {n} | {minc} |")
            continue
        n = sum(counts.get(cat, {}).values())
        lines.append(f"| {cat} | {n} | {minc} |")
    lines.append("")
    lines.append(f"Tasks: {len(TASKS)} · Queries: {len(QUERIES)}")
    lines.append("")
    return "\n".join(lines)


def main() -> None:
    problems = check_minimums()
    if problems:
        raise SystemExit("Fixture below §10 minimums:\n  " + "\n  ".join(problems))

    fixture = {
        "format_version": 1,
        "note": "Synthetic RFC 0007 §10 evaluation fixture",
        "tasks": TASKS,
        "queries": QUERIES,
    }
    OUT.write_text(json.dumps(fixture, indent=2) + "\n")
    MD.write_text(render_markdown() + "\n")
    print(f"wrote {OUT} ({len(TASKS)} tasks, {len(QUERIES)} queries)")
    for cat, per in count_by_category().items():
        print(f"  {cat}: {per}")


if __name__ == "__main__":
    main()
