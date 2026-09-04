# Memory — orion-test

> Generated: 2026-09-04 19:51:24  
> Total memories: **48**  
> Breakdown: instruction: 3, fact: 2, decision: 9, goal: 2, preference: 4, context: 10, event: 5, learning: 9, artifact: 4

---

## Instructions

*Standing rules, constraints, and guidelines to always follow.*

### User rule: always use the latest stable version of...

> User rule: always use the latest stable version of Apache Airflow (3.x) for mill-tower, not 2.x. Mark questioned why 2.10 was proposed (2026-09-03).

*Confidence: 1.0 | Status: active | Created: 2026-09-02T18:50:25*

### User rule (2026-09-03, strongly stated): NEVER ren...

> User rule (2026-09-03, strongly stated): NEVER rename/suffix DAG ids with a system name; one DAG mixes Linux/SQL Server/Windows. The system belongs on task ids (_linux/_sqlserver/_windows, bridges _sqlserver_to_windows) with task_display_name like 'load_to_core [Linux]'. DAG display names may mention systems.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T20:00:20*

### User rule for mill-tower: no mocks or simulated ex...

> User rule for mill-tower: no mocks or simulated execution in demos. Every scheduler source (including Windows Task Scheduler) must execute for real on a real target.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:48:27*

---

## Facts

*Verified information, project status, and established truths.*

### Plane editions (2026-09-04): Community (makeplane/...

> Plane editions (2026-09-04): Community (makeplane/plane, AGPL, v1.x) and Commercial (private makeplane/plane-ee, semver v3.x) are officially 'distinct codebases' with three release cycles: Cloud first, then Commercial, then Community; Community lags. Commercial has a free tier with 12 seats/workspace. Core UI/data model are the same lineage so Commercial is a good preview of features Slate must build itself.

*Confidence: 0.9 | Status: active | Created: 2026-09-04T09:53:07*

### Plane has an official desktop app (Electron 41, v2...

> Plane has an official desktop app (Electron 41, v2.0.0 on macOS/Linux, Windows lags on v1.6.1) downloadable from plane.so/download; it is closed-source (not in the makeplane/plane repo) and connects to self-hosted instances. Mobile app source is at github.com/makeplane/plane-mobile. Slate desktop app would need to be built separately (Electron/Tauri wrapper). Noted 2026-09-04.

*Confidence: 0.9 | Status: active | Created: 2026-09-04T09:23:28*

---

## Decisions

*Architectural choices, approach selections, and their rationale.*

### docgen gantt charts are now written in markwhen sy...

> docgen gantt charts are now written in markwhen syntax (markwhen fenced code block). The official @markwhen/parser was rejected: it is a Node package, silently drops group/endGroup in v1.2.0 on npm, and its renderer is a Vue web app needing a headless browser to produce a PNG. So docgen implements the markwhen date grammar in Python (docgen/gantt.py) and keeps the deterministic matplotlib renderer - verified identical to the real parser on all 9 date forms. The legacy pipe spec (name | start | duration | kind) still parses and renders byte-identical.

*Confidence: 0.95 | Status: active | Created: 2026-09-04T07:31:19*

### mill-tower runs Apache Airflow 3.3.1 (latest stabl...

> mill-tower runs Apache Airflow 3.3.1 (latest stable as of 2026-09-03) via docker-compose: airflow-apiserver (:8081), scheduler, dag-processor, FabAuthManager, JWT secret in compose. REST is /api/v2, token from POST /auth/token. DAG files use airflow.sdk. All three sources (crontab, SQL Agent, Windows GCP host) re-verified success on 3.3.1.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T18:55:00*

### Slate roadmap decided 2026-09-04: Phase 1 = Compan...

> Slate roadmap decided 2026-09-04: Phase 1 = Company/Project Management, combining Jira + Notion + ClickUp features on top of the Plane fork. Phase 2 = Communication layer (Slack/Teams-like chat). Chat is deferred; do not design Phase 1 around it.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:00:38*

### Mark chose GCP (project codefin-lab) over AWS for ...

> Mark chose GCP (project codefin-lab) over AWS for provisioning the mill-tower Windows demo host via Terraform, 2026-09-03.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:58:11*

### mill-tower architecture implication: the visual wo...

> mill-tower architecture implication: the visual workflow builder is the primary authoring surface, so DAGs must be generated from a mill-tower workflow model (stored in mill-tower DB) rather than hand-written in dags/. Legacy scheduler jobs (Task Scheduler / SQL Agent / crontab) enter the system through importers that map to the same workflow model.

*Confidence: 0.9 | Status: active | Created: 2026-09-02T17:35:40*

### mill-tower scaffold decided 2026-09-03: monorepo w...

> mill-tower scaffold decided 2026-09-03: monorepo with web/ (React 19 + Vite + TS + Tailwind v4 + shadcn/ui via pnpm), server/ (FastAPI + httpx AirflowClient over Airflow REST v1, managed by uv, tests mock Airflow with respx), dags/ mounted into docker-compose Airflow 2.10. Vite proxies /api to FastAPI :8000. shadcn CLI hangs on this machine so ui components are hand-written.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:25:13*

### Orion AI memory layer will run fully on-prem: Moor...

> Orion AI memory layer will run fully on-prem: Moorcheh server in Docker, Ollama on the host with nomic-embed-text for embeddings and qwen2.5:14b for LLM. Cloud backend is not allowed.

*Confidence: 0.8 | Status: active | Created: 2026-09-02T11:27:25*

### mill-tower web graph uses @dagrejs/dagre (rankdir ...

> mill-tower web graph uses @dagrejs/dagre (rankdir LR, network-simplex) for DAG layout since 2026-09-03; nodes sized to labels; alert_on_failure drawn as dashed sink without its fan-in edges. User wants graphs laid out cleanly with no overlaps.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T20:07:56*

### mill-tower (2026-09-03): DAGs are business-named (...

> mill-tower (2026-09-03): DAGs are business-named (partner_application_intake, premium_calculation, policy_esignature, ...), defined in dags/jobs/<dag_id>.json with that job's real steps (3-8 operators, mixed step types ssh/sql/winrm/agent_job/schtask/bridges in one DAG). Workflows nest business DAGs as TaskGroups: new_business_policy_issuance = 46 tasks, verified 45 success + alert skipped. User rules: no system prefixes in DAG names; do not normalise DAGs to one template; a DAG may mix Linux/SQL/Windows steps.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:38:12*

---

## Goals

*Objectives, targets, and milestones to track progress.*

### Slate goal: replace Jira for the team with a self-...

> Slate goal: replace Jira for the team with a self-hosted product built by forking Plane; feature sources to study: Jira (issue tracking), Notion (docs/wiki), ClickUp (views/tasks), Microsoft Teams and Slack (chat/collaboration).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T08:58:09*

### Customer requirement for mill-tower (stated 2026-0...

> Customer requirement for mill-tower (stated 2026-09-03): must support three scheduler targets: Windows Task Scheduler, SQL Server task scheduler (SQL Agent jobs), and Linux crontab. Scope (migrate into Airflow vs orchestrate/monitor existing schedulers) not yet decided.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:35:06*

---

## Commitments

*Promises, obligations, and TODOs that need follow-through.*

*No memories of this type.*

---

## Preferences

*User and entity preferences for personalization.*

### Mark prefers concise Thai replies with code in fen...

> Mark prefers concise Thai replies with code in fenced blocks.

*Confidence: 0.8 | Status: active | Created: 2026-09-02T11:27:25*

### User (2026-09-03) removed the Import cards from th...

> User (2026-09-03) removed the Import cards from the Sources page and wants the mill-tower UI to mirror Airflow's DAG views with realtime status changes.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:53:36*

### User (Mark) expects every DAG, not only workflows,...

> User (Mark) expects every DAG, not only workflows, to be a multi-operator graph; a single 'run' task per DAG was rejected (2026-09-03).

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:30:08*

### User wants workflow graph views to show many nodes...

> User wants workflow graph views to show many nodes with clear directed dependencies (fan-out/fan-in), not a single chain (2026-09-03).

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:23:02*

---

## Relationships

*Entity connections, team context, and collaboration patterns.*

*No memories of this type.*

---

## Context

*Session summaries, status updates, and conversation state.*

### mill-tower builds on Apache Airflow

> mill-tower (repo at ~/Labs/mill-tower) is a system that leverages Apache Airflow as its foundation and adds extra capabilities on top of it. As of 2026-09-03 the directory is empty (new project, not yet a git repo).

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:13:40 | Tags: `mill-tower`, `airflow`, `project`*

### mill-tower DAG detail (2026-09-03) tabs: Overview ...

> mill-tower DAG detail (2026-09-03) tabs: Overview (SLA tile+countdown, cutover tile, success rate, durations full-width chart, slowest steps, business outputs from XCom, legacy job card), Runs & Tasks (graph on top, runs, task instances with Retry/Mark success/Mark failed), Dependencies (lineage graph within workflow, systems, data hand-offs), Calendar (past run states + future schedule via croniter), Audit Log, Settings (cutover switch-off/rollback acting on real legacy schedulers; SLA + alert rules form), Definition. Separate Graph tab removed per user.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T20:25:04*

### mill-tower product positioning (told to customer 2...

> mill-tower product positioning (told to customer 2026-09-03): Centralized Job Scheduling & Orchestration Platform built on Apache Airflow, developed as a product. Consolidates jobs from Windows Task Scheduler, SQL Agent jobs, and Linux crontab under one management plane. Has a Drag & Drop Workflow builder to create and control jobs without writing Airflow DAGs directly, plus Monitoring, Logs, Retry, Alerting, and Dependency Management.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:35:39*

### mill-tower: every generated job DAG has 5 operator...

> mill-tower: every generated job DAG has 5 operators: preflight (target reachable, job exists/enabled) -> run (blocking executor) -> verify (evidence: log tail / Agent history / LastTaskResult) -> audit (dags/audit/*.jsonl) plus alert_on_failure (trigger_rule one_failed, dags/alerts/alerts.jsonl). In workflows each job is a TaskGroup with preflight/run/verify; policy_issuance workflow = 32 tasks. Implemented in dags/mill_tower_lib/steps.py (2026-09-03).

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:30:08*

### mill-tower web (2026-09-03): DAGs page mirrors Air...

> mill-tower web (2026-09-03): DAGs page mirrors Airflow via REST v2 (list: tags, schedule, next run, last run, recent-run bars, pause/unpause, trigger; detail /dags/:id: graph built from Airflow task structure with TaskGroup colouring, runs list, task instances with state/duration, task log viewer). Workflows page removed at user's request; graphs live in Airflow and in DAG detail. Sources page links each imported job to its business DAG.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:43:41*

### mill-tower demo scenario (2026-09-03): insurance n...

> mill-tower demo scenario (2026-09-03): insurance new-business. Hosts: Linux uw-batch-01 (cron: ingest, risk scoring, notify, OIC report; scripts use pymssql into SQL Server), SQL-CORE-01 (InsuranceCore DB; Agent jobs UW_Calculate_Premium, UW_Underwriting_Decision, POL_Issue_Policies, FIN_Nightly_Reserves), Windows DOC-SRV-01 on GCP (Task Scheduler \\Insure\\: GeneratePolicyDocuments, SendForESignature, ArchiveSignedDocuments; installed by scripts/windows-setup.py). Workflow dags/workflows/policy_issuance.json = 15 steps with fan-out/fan-in; verified 15/15 success end-to-end. Old ERP/Corp samples and example_hello removed.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:23:02*

### Project Slate (~/Labs/slate, started 2026-09-04): ...

> Project Slate (~/Labs/slate, started 2026-09-04): team is moving off Jira to a self-hosted tool. Plan is to fork Plane (makeplane/plane) and customise it into a product named Slate, combining features from Jira, Notion, ClickUp, Microsoft Teams and Slack. Directory is empty as of 2026-09-04 (no git repo, no fork yet).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T08:58:09*

### mill-tower DAG detail page (2026-09-03) has tabs O...

> mill-tower DAG detail page (2026-09-03) has tabs Overview (stat tiles: last run, success rate + 7d, runs 24h, avg/max duration, next run, last failure; duration bar chart; slowest steps; legacy-job card), Graph (dagre), Runs & Tasks (+ log viewer), Audit Log (Airflow eventLogs + mill-tower alerts + run ledger written by DAG on_success/on_failure callbacks), Definition (JSON spec). API: /api/dags/{id}/stats|audit|definition.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T20:15:21*

### mill-tower Windows host is provisioned with Terraf...

> mill-tower Windows host is provisioned with Terraform in infra/gcp-windows (GCP project codefin-lab, asia-southeast1-b, e2-medium win-app-01, WinRM 5985 NTLM open to operator IP only). terraform apply must be run by Mark himself: Claude Code auto-mode classifier blocks it. After apply run scripts/windows-host-env.sh.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T18:01:59*

### mill-tower Windows path (2026-09-03): no simulatio...

> mill-tower Windows path (2026-09-03): no simulation. Importer uses pywinrm + Export-ScheduledTask; DAG uses WinRMOperator running schtasks /Run + poll. Needs a real Windows host in .env (WIN_HOST/USER/PASSWORD, WinRM HTTP 5985 NTLM) which Mark has not yet provided. Setup steps in docs-windows-host.md.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:51:40*

---

## Events

*Important conversations, milestones, and temporal occurrences.*

### memanto CLI is installed and working on Mark's mac...

> memanto CLI is installed and working on Mark's machine as of 2026-09-03 (moorcheh-client installed); use CLI via Bash, not MCP fallback

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:14:58*

### mill-tower Windows source verified 2026-09-03 on G...

> mill-tower Windows source verified 2026-09-03 on GCP VM win-app-01 (34.124.151.45, asia-southeast1-b): WinRM user is 'milltower' (built-in Administrator cannot be enabled on GCE images; use gcloud compute reset-windows-password or the bootstrap's New-LocalUser). Airflow WinRMOperator runs schtasks /Run then polls Get-ScheduledTaskInfo; NightlyBackup returned LastTaskResult=0 and wrote C:\Scripts\backup.log. VM is billed while it exists; terraform destroy in infra/gcp-windows when done.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T18:39:23*

### Phase 6.10 done (2026-09-03): tools/linearize.py c...

> Phase 6.10 done (2026-09-03): tools/linearize.py checks linearizability against three live nodes with chaos cuts (POST /_boost/chaos with BOOSTSEARCH_CHAOS=1) and SIGSTOP; a cut drops frames silently like a real partition; copy writes wait while the node is a member and give up as 'node left'; in-sync allocation ids carried in metadata with SHARD_STALE reports; health wait_for_nodes compares the live node count. No acknowledged write lost across seeds 2-4; stale reads only inside fault windows under the shipped OpenSearch mode.

*Confidence: 0.95 | Status: active | Created: 2026-09-03T01:23:19*

### BoostSearch Phase 6 complete through 6.12 (2026-09...

> BoostSearch Phase 6 complete through 6.12 (2026-09-03): cluster chaos harness tools/cluster_chaos.py (chaos/rolling/soak modes, telling a lost write from a copy that is behind), tools/rolling_upgrade.py (two builds, node by node), and the OpenSearch corpus run against three nodes (core 1412/1427, module 813/895; diffs 92/92, 28/29, 520/522). Ten data-loss bugs fixed, the last being a stale primary poisoning the in-sync set and version-vs-term divergence between copies.

*Confidence: 0.95 | Status: active | Created: 2026-09-03T11:28:50*

### 2026-09-04: Mark decided to study Plane Commercial...

> 2026-09-04: Mark decided to study Plane Commercial Edition first before customising the Community fork. Community stack (compose project 'slate') was stopped (data kept) to free port 80. prime-cli v2.2.0 placed at ~/.local/bin/prime-cli; it installs to /opt/plane, needs sudo and an interactive TUI, so Mark must run 'sudo ~/.local/bin/prime-cli setup --domain localhost' himself (auto-mode cannot). Restart Community with: cd ~/Labs/slate/deployments/cli/community && docker compose -p slate start (after stopping Commercial with prime-cli stop, both want port 80).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:39:34*

---

## Learnings

*Knowledge acquired from experience, corrections, and insights.*

### Lesson (2026-09-03): a ResizeObserver-driven SVG w...

> Lesson (2026-09-03): a ResizeObserver-driven SVG with pixel width inside a CSS grid 1fr track causes an infinite layout loop that hangs the tab (1fr = minmax(auto,1fr) grows with content). Use svg width=100% + viewBox and grid minmax(0,1fr) / min-w-0.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T20:27:03*

### docgen: base.docx sets Normal to before:0/after:0,...

> docgen: base.docx sets Normal to before:0/after:0, so EVERY vertical gap in a generated proposal comes from theme.yaml "space:" (paragraph_after, list_item_after, list_after, table_after, image_after, bold_lead_before). A blank line in the markdown produces no visible gap without it. Also added: table columns sized by content weight (table.column_widths: content) instead of equal, never narrower than the longest word in the column; page.keep_lead_with_next stops orphaned lead-ins; parser accepts Word-pasted bullets (bullet, middot, square, circle) as list markers.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:02:37*

### Lesson: when a compose service is renamed/removed,...

> Lesson: when a compose service is renamed/removed, old containers become orphans that compose commands cannot address and they keep holding ports; remove them with docker rm -f <container>. Also stop postgres before removing its volume.

*Confidence: 0.9 | Status: active | Created: 2026-09-02T18:55:00*

### Lesson: on Mark's Mac the shadcn CLI hangs and mss...

> Lesson: on Mark's Mac the shadcn CLI hangs and mssql-tools image mcr.microsoft.com/mssql-tools18/mssql-tools does not exist; use the mssql/server image itself for sqlcmd. Airflow provider versions must not be pinned when installing under the Airflow constraints file.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T17:46:55*

### mill-tower realtime (2026-09-03): /api/events SSE ...

> mill-tower realtime (2026-09-03): /api/events SSE from one shared watcher (events.py) polling Airflow every 2s with batched requests; web useLiveUpdates hook updates TanStack cache + toasts. Lesson: per-tab pollers hitting Airflow v2 API exhausted the api-server SQLAlchemy pool (TimeoutError) and hung it; fixed by shared watcher, /dags/~/dagRuns batch endpoint, and POOL_SIZE 20 / MAX_OVERFLOW 30. Also stale uvicorn processes kept running after pkill -f 'uvicorn mill_tower'; use pkill -9 -f uvicorn.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T19:53:35*

### Lesson: WinRMOperator in airflow-providers-microso...

> Lesson: WinRMOperator in airflow-providers-microsoft-winrm (Airflow 2.10 constraints) has no expected_return_code arg; and when passing Windows paths through curl JSON use a single escaped backslash ("\\Corp\\" in shell = \Corp\).

*Confidence: 0.95 | Status: active | Created: 2026-09-02T18:39:23*

### Lessons (2026-09-03): Airflow 3 creates DAGs pause...

> Lessons (2026-09-03): Airflow 3 creates DAGs paused by default -> set is_paused_upon_creation=False for generated workflow DAGs. Windows Server 2022 ships PowerShell 5.1: no ConvertTo-Json -AsArray. WinRM -EncodedCommand limit ~8k chars: upload long scripts in 2000-char base64 chunks then run with -File. sp_start_job is async: poll sysjobactivity/sysjobhistory to make dependencies real.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T19:23:03*

### docgen supports table banner rows: a table row who...

> docgen supports table banner rows: a table row whose ONLY non-empty cell holds a **bold** label is merged (w:gridSpan) into a full-width shaded group heading. Requiring bold is deliberate - inferring banners from "only one cell has text" would swallow real data rows with an empty number cell. Implementation gotcha: set gridSpan AFTER capturing the raw w:tc elements, because python-docx row.cells repeats a merged cell once per spanned column, so deleting "cells after the first" through that view deletes the first cell again.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:10:16*

### Plane desktop app (v2.0.0) refuses Community Editi...

> Plane desktop app (v2.0.0) refuses Community Edition: error 'Your Plane instance is on v1.4.2... Upgrade to Plane Commercial v3.0.0 or later'. Our fork is Community v1.4.2 (AGPL). Commercial edition is closed-source, installed via prime-cli, and is where desktop app, workflows/approvals, SSO, epics, integrations live. Desktop app cannot be used with Slate; Slate needs its own Electron/Tauri wrapper. Observed 2026-09-04.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:30:36*

---

## Observations

*Patterns noticed, behavioral notes, and recurring themes.*

*No memories of this type.*

---

## Artifacts

*Tool outputs, files, reports, and external references.*

### mill-tower runtime architecture diagram lives at d...

> mill-tower runtime architecture diagram lives at docs/architecture.html (archify, spec docs/architecture.archify.json, showcase profile passed 9/9 checks, delivered 2026-09-03). Repo has no GitHub remote so archify 'sources' evidence cannot be pinned.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T20:39:34*

### mill-tower demo built 2026-09-03 (commit after a07...

> mill-tower demo built 2026-09-03 (commit after a07ca49): docker-compose runs Airflow at :8081 (port 8080 is occupied by moorcheh-onprem-server, never reuse it), cron-host container (ssh :2222 demo/demo, real crontab), SQL Server 2022 amd64 with Agent jobs (:1433 sa/MillTower!2026, init via docker compose run --rm mssql-init), Windows tasks as exported XML in demo/windows (execution simulated, WinRM in prod). Importers -> JobSpec -> dags/imported/*.json -> dags/mill_tower_imported.py generates DAGs. All three sources verified running end-to-end. scripts/demo.sh brings it up.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:46:54*

### Slate local self-host (2026-09-04): Plane cloned i...

> Slate local self-host (2026-09-04): Plane cloned into ~/Labs/slate (shallow, remote named 'upstream' -> makeplane/plane, commit da1a7ab 2026-09-02). Runs with prebuilt images via: cd deployments/cli/community && docker compose -p slate --env-file .env -f docker-compose.yml up -d (.env copied from variables.env with generated SECRET_KEY/LIVE_SERVER_SECRET_KEY, gitignored). Serves http://localhost (port 80): / web, /god-mode/ admin, /spaces/ public, /api/. Root docker-compose.yml builds from source and is the path for actual Slate development. Plane license is AGPL-3.0.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:04:49*

### Slate desktop shell created 2026-09-04 at apps/des...

> Slate desktop shell created 2026-09-04 at apps/desktop (Electron 41, npm, not yet in pnpm workspace deps): main.js loads SLATE_URL (default http://localhost), same-origin nav stays in window, external links open in browser. Run: cd apps/desktop && npm install && SLATE_URL=http://localhost npm start. Created because Plane's official desktop app refuses Community edition. Also: Docker Desktop VM died with I/O errors on 2026-09-04; fix was pkill -f com.docker.backend + pkill -x 'Docker Desktop' then open -a Docker (osascript quit alone left backend hung). Container os-compat failed to restart afterwards.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T12:09:30*

---

## Errors

*Failure records, bugs, and lessons learned from mistakes.*

*No memories of this type.*

---

*End of memory export.*
