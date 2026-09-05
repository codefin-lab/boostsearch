# Memory — orion-test

> Generated: 2026-09-05 14:50:54  
> Total memories: **86**  
> Breakdown: instruction: 3, fact: 2, decision: 17, goal: 2, commitment: 1, preference: 10, context: 11, event: 8, learning: 21, artifact: 11

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

### Slay icon v6 cleanup (2026-09-05): Mark asked to r...

> Slay icon v6 cleanup (2026-09-05): Mark asked to remove AI-trace artifacts ('ติ่ง'). Done in gen_icon.py: twist (with its 2.5px expansion stroke) clipped to the silhouette union (silClip) so no nubs at the concave corner or crease end; BEND vertex removed, lower panel's upper-left edge is now straight and parallel to the top panel's right edge (37.5 deg), NOTCH=(434,509) leaving ~11px gap from the fold arc end; corner radii made symmetric (tips 21, acute 62, obtuse 76). Deliberately deviates from the reference (IoU 0.966).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:41:58*

### Slay logo mark decision (2026-09-05): Mark chose t...

> Slay logo mark decision (2026-09-05): Mark chose the 'gap' flat mark (top panel separated from the lower ribbon by a 28px mask stroke). logo-mark-black/white.svg+png are now this variant; knockout and twotone removed from gen_icon.py.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:51:50*

### mill-tower runs Apache Airflow 3.3.1 (latest stabl...

> mill-tower runs Apache Airflow 3.3.1 (latest stable as of 2026-09-03) via docker-compose: airflow-apiserver (:8081), scheduler, dag-processor, FabAuthManager, JWT secret in compose. REST is /api/v2, token from POST /auth/token. DAG files use airflow.sdk. All three sources (crontab, SQL Agent, Windows GCP host) re-verified success on 3.3.1.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T18:55:00*

### Slate roadmap decided 2026-09-04: Phase 1 = Compan...

> Slate roadmap decided 2026-09-04: Phase 1 = Company/Project Management, combining Jira + Notion + ClickUp features on top of the Plane fork. Phase 2 = Communication layer (Slack/Teams-like chat). Chat is deferred; do not design Phase 1 around it.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:00:38*

### mill-tower architecture implication: the visual wo...

> mill-tower architecture implication: the visual workflow builder is the primary authoring surface, so DAGs must be generated from a mill-tower workflow model (stored in mill-tower DB) rather than hand-written in dags/. Legacy scheduler jobs (Task Scheduler / SQL Agent / crontab) enter the system through importers that map to the same workflow model.

*Confidence: 0.9 | Status: active | Created: 2026-09-02T17:35:40*

### Decision 2026-09-05: Mark chose Tauri (v2) over El...

> Decision 2026-09-05: Mark chose Tauri (v2) over Electron for the Slay desktop shell, goals: small binary, fast start, low RAM; wants Slay's selling points over Plane defined and continued optimisation. Electron shell in apps/desktop stays as reference until the Tauri shell reaches tab parity (multiwebview needs Tauri's 'unstable' feature).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T22:14:40*

### Codefin cover artwork is now REDRAWN, not the Goog...

> Codefin cover artwork is now REDRAWN, not the Google Docs raster: tools/redraw_cover_art.py holds the line geometry (recovered by cv2 HoughLinesP over the original PNGs, endpoints snapped so shared vertices meet) and renders both drawings with matplotlib at one width (0.5pt), one colour and one opacity (0.42). Reason: in the export PNG each stroke width depends on its angle - a scanline through image1 spans 1-18px with per-stroke peak alpha 87-119 - and the two drawings disagreed with each other (peak alpha 119 vs 51). That unevenness is baked into the pixels and no post-processing fixes it. Run boost_cover_art.py (z-order) then redraw_cover_art.py after any make_base.py.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:17:25*

### Mark chose GCP (project codefin-lab) over AWS for ...

> Mark chose GCP (project codefin-lab) over AWS for provisioning the mill-tower Windows demo host via Terraform, 2026-09-03.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:58:11*

### Orion AI memory layer will run fully on-prem: Moor...

> Orion AI memory layer will run fully on-prem: Moorcheh server in Docker, Ollama on the host with nomic-embed-text for embeddings and qwen2.5:14b for LLM. Cloud backend is not allowed.

*Confidence: 0.8 | Status: active | Created: 2026-09-02T11:27:25*

### mill-tower scaffold decided 2026-09-03: monorepo w...

> mill-tower scaffold decided 2026-09-03: monorepo with web/ (React 19 + Vite + TS + Tailwind v4 + shadcn/ui via pnpm), server/ (FastAPI + httpx AirflowClient over Airflow REST v1, managed by uv, tests mock Airflow with respx), dags/ mounted into docker-compose Airflow 2.10. Vite proxies /api to FastAPI :8000. shadcn CLI hangs on this machine so ui components are hand-written.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:25:13*

### Codefin proposal type scale widened: size h1 20 / ...

> Codefin proposal type scale widened: size h1 20 / h2 13 / h3 11.5 / body 10 (was 18/16/12). At 18 vs 16 with the same font, weight and colour, h1 and h2 were separated only by h1 being ALL CAPS - the hierarchy did not read. Also added: table.keep_rows_whole (w:cantSplit on every row - a row cut in half by a page break reads as a mistake; check the tallest row first, DAOL max is ~4in so nothing is stranded) and page.keep_intro_with_list + intro_keep_max_chars 90 (a SHORT line introducing a list or table gets keepNext; the length limit matters because keepNext moves the whole paragraph and would drag body prose along).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T22:20:37*

### Slay icon v5 FINAL structure (2026-09-05): Mark re...

> Slay icon v5 FINAL structure (2026-09-05): Mark rejected the tuck-under redesign ('worse, original shape was right; keep structure, only fix the black eating in'). gen_icon.py: top panel notched by explicit circular fold arc (FOLD_A 422,434 -> FOLD_B 423,501, r44.7), twist+lower share the same arc, lower panel's upper-left edge (BEND 304,583) ends at FOLD_B so the gap wedge stops at the arc end, reference-strength shadow band (op0.5 w80 blur28, base #6b7886) so the twist reads as passing under the panel. IoU 0.981, err 4.1/255.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:18:43*

### mill-tower web graph uses @dagrejs/dagre (rankdir ...

> mill-tower web graph uses @dagrejs/dagre (rankdir LR, network-simplex) for DAG layout since 2026-09-03; nodes sized to labels; alert_on_failure drawn as dashed sink without its fan-in edges. User wants graphs laid out cleanly with no overlaps.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T20:07:56*

### mill-tower (2026-09-03): DAGs are business-named (...

> mill-tower (2026-09-03): DAGs are business-named (partner_application_intake, premium_calculation, policy_esignature, ...), defined in dags/jobs/<dag_id>.json with that job's real steps (3-8 operators, mixed step types ssh/sql/winrm/agent_job/schtask/bridges in one DAG). Workflows nest business DAGs as TaskGroups: new_business_policy_issuance = 46 tasks, verified 45 success + alert skipped. User rules: no system prefixes in DAG names; do not normalise DAGs to one template; a DAG may mix Linux/SQL/Windows steps.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:38:12*

### Codefin proposal cover design (docgen): hierarchy ...

> Codefin proposal cover design (docgen): hierarchy is theme-driven via theme.yaml cover: block (eyebrow / title+rule / meta / label / value / brand / brand_sub / legal) applied to base.docx at build time by apply_cover_style, matching roles on paragraph text. The Google Docs export ships almost every cover line at 14pt so nothing has rank until this runs. Cover artwork needs tools/boost_cover_art.py: the line drawings are 1px hairlines, peak alpha 119 median 53 - scaling alpha LINEARLY is invisible because most pixels sit near the old median, so it thickens the stroke (MaxFilter 5) and lifts midtones with gamma 0.55. The bottom-right drawing is REMOVED entirely: it sits under the CODEFIN logo and address, and any weight that makes it read as texture cuts through the logo.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:05:51*

### 2026-09-05: Mark renamed the project from Slate to...

> 2026-09-05: Mark renamed the project from Slate to Slay. Same repo dir ~/Labs/slate for now (directory not renamed). Community stack now runs as compose project 'slay' with volumes slay_* (copied from slate_*; old slate_* volumes kept as backup). Desktop shell at apps/desktop is now @plane/slay-desktop, env SLAY_URL, config ~/.slay-desktop.json, app name Slay. Commercial stack (plane-commercial) stopped.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:17:23*

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

### Handoff document for the Slay directory rename wri...

> Handoff document for the Slay directory rename written 2026-09-05 at /private/tmp/claude-501/-Users-mark-Labs-slate/5f4fc33e-1d8f-4877-92a0-3e461219de92/scratchpad/HANDOFF-slay.md (session temp dir; may be cleaned). Next session: mv ~/Labs/slate ~/Labs/slay, resync memanto MEMORY.md in the new dir, then continue Commercial-vs-Community gap study.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:18:51*

---

## Preferences

*User and entity preferences for personalization.*

### Slay icon v5.8 (2026-09-05): Mark asked to bring t...

> Slay icon v5.8 (2026-09-05): Mark asked to bring the rim + crease lines back after trying without them: white 1px visible (2px stroke clipped inside the panel), opacity 0.5, on top-panel edge+fold arc, lower-panel top edge, and crease; combined with the softened twist shadow (edge ~124).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:38:54*

### Slay icon v5.5 (2026-09-05): Mark asked to remove ...

> Slay icon v5.5 (2026-09-05): Mark asked to remove all edge highlight strokes entirely (no rim on top panel, lower panel, or crease). gen_icon.py now draws only fills, gradients, twist shadow band and drop shadows.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:34:22*

### Slay icon v5.3 (2026-09-05): per Mark, all three e...

> Slay icon v5.3 (2026-09-05): per Mark, all three edge highlights (top-panel rim incl. fold arc, lower-panel top rim, crease) are uniform 1px white at opacity 0.5.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:31:37*

### Mark prefers concise Thai replies with code in fen...

> Mark prefers concise Thai replies with code in fenced blocks.

*Confidence: 0.8 | Status: active | Created: 2026-09-02T11:27:25*

### User (2026-09-03) removed the Import cards from th...

> User (2026-09-03) removed the Import cards from the Sources page and wants the mill-tower UI to mirror Airflow's DAG views with realtime status changes.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:53:36*

### Slay icon v3.2 (2026-09-05): Mark still saw the tw...

> Slay icon v3.2 (2026-09-05): Mark still saw the twist shadow as 'black eating the ribbon' even after it matched the reference numerically, so the default in gen_icon.py is now deliberately softer than the AI reference: band_op 0.28 (w70 blur26), base twist start #939ca6, shadow band stops at the fold arc (no longer runs along the bottom edge, which had doubled the darkness at the fold), plus a 2.5px light rim around the fold. Reference-exact values kept as a comment (band_op 0.5, tw0 #6b7886). Preference: Mark values a clean soft look over pixel-exact match once shapes are right.

*Confidence: 0.9 | Status: active | Created: 2026-09-04T21:08:02*

### Slay icon v5.7 (2026-09-05): Mark kept seeing a 'b...

> Slay icon v5.7 (2026-09-05): Mark kept seeing a 'black edge' where the twist meets the top panel; it was the shadow itself (edge value 68 vs panel 185, same as reference). Softened on purpose: band_op 0.3 blur 30 offset (9,12), twist base start #9aa3ac -> edge ~124 gray. Mark prefers soft, low-contrast shading over reference-exact darkness.

*Confidence: 0.95 | Status: active | Created: 2026-09-04T21:37:40*

### User (Mark) expects every DAG, not only workflows,...

> User (Mark) expects every DAG, not only workflows, to be a multi-operator graph; a single 'run' task per DAG was rejected (2026-09-03).

*Confidence: 1.0 | Status: active | Created: 2026-09-02T19:30:08*

### Codefin cover artwork decision (corrected): the tw...

> Codefin cover artwork decision (corrected): the two geometric line drawings stay THIN and FAINT exactly as the Google Docs export made them - the owner wants them subtle, not boosted. tools/boost_cover_art.py defaults to restoring the export originals and only fixes z-order (the bottom-right drawing ships in FRONT of the text and crosses the disclaimer, address and logo; every floating anchor is set behindDoc=1). An optional --peak/--grow pass exists but is NOT the house look. Earlier attempts to darken and then to delete the bottom-right drawing were both rejected.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:09:34*

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

### Slay web rebrand (2026-09-05, in progress): gen_ic...

> Slay web rebrand (2026-09-05, in progress): gen_icon.py now also writes packages/propel/src/icons/brand/plane-{logo,lockup,wordmark}.tsx (names kept, Slay geometry inside, mask ids slay-mark-gap / slay-lockup-gap), apps/admin/components/common/plane-lockup.tsx, all favicon/PWA PNG+ICO in apps/{web,admin,space}, plane-logos horizontal PNGs, apps/space/app/assets/plane-logo.svg, apps/api/plane/static/logos/Logo.png (email). Manifests renamed to Slay. Local pnpm is broken (wants v11.10.0 not installed; no node_modules), so validation happens via docker build: /tmp/build-slay-frontends.sh builds makeplane/plane-{frontend,admin,space}:slay and tags backend/live :slay; then set APP_RELEASE=slay in deployments/cli/community/.env and recreate the stack.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T22:07:25*

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

### Slay web rebrand DONE (2026-09-05): stack at http:...

> Slay web rebrand DONE (2026-09-05): stack at http://localhost now runs locally built images makeplane/plane-{frontend,admin,space}:slay (APP_RELEASE=slay in deployments/cli/community/.env; backend/live/proxy :slay are retags of stable). Slay logo lockup shows on web sign-in and god-mode; favicon served = Slay. Build fixes committed to working tree: pnpm-workspace.yaml excludes apps/desktop; .dockerignore ignores apps/desktop/; apps/space/Dockerfile.space installs from full workspace with --no-frozen-lockfile because pnpm 11.10 falsely reports a phantom @makeplane/propel@0.2.0 lockfile entry under fetch/frozen (web/admin prune builds are fine). Propel lockup TSX uses a tight viewBox so it fills the 95x20 header box. Rebuild cmd: /tmp/build-fe2.sh pattern = docker build -f apps/<app>/Dockerfile.<app> -t makeplane/plane-<img>:slay . then docker compose -p slay up -d web admin space. Text strings ('Plane') in UI/titles not changed yet.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T22:39:09*

### Plane Commercial v3.1.4 running locally since 2026...

> Plane Commercial v3.1.4 running locally since 2026-09-05 (compose project plane-commercial, ~/plane-commercial, http://127.0.0.1, edition PLANE_COMMERCIAL, 21 containers incl. silo/runner/monitor/iframely). Required MACHINE_SIGNATURE=uuid in plane.env or the monitor container crash-loops. First visit /god-mode/ to create instance admin (is_setup_done was false). Desktop app should be pointed at http://127.0.0.1.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T19:57:08*

### 2026-09-04: Mark decided to study Plane Commercial...

> 2026-09-04: Mark decided to study Plane Commercial Edition first before customising the Community fork. Community stack (compose project 'slate') was stopped (data kept) to free port 80. prime-cli v2.2.0 placed at ~/.local/bin/prime-cli; it installs to /opt/plane, needs sudo and an interactive TUI, so Mark must run 'sudo ~/.local/bin/prime-cli setup --domain localhost' himself (auto-mode cannot). Restart Community with: cd ~/Labs/slate/deployments/cli/community && docker compose -p slate start (after stopping Commercial with prime-cli stop, both want port 80).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:39:34*

### 2026-09-05: Repo directory renamed ~/Labs/slate ->...

> 2026-09-05: Repo directory renamed ~/Labs/slate -> ~/Labs/slay (done by Mark). Verified: remote upstream=makeplane/plane at da1a7ab, Community stack (compose project slay) running on http://localhost, slay_* volumes present, old slate_* volumes still kept as backup. No 'slate' strings remain in repo (apart from MEMORY.md and an unrelated 'clean slate' phrase in apps/api/tests/RUNNING_TESTS.md). MEMORY.md resynced in new dir. Commands now: cd ~/Labs/slay/deployments/cli/community && docker compose -p slay ...

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:26:44*

---

## Learnings

*Knowledge acquired from experience, corrections, and insights.*

### Slay icon v5.2 (2026-09-05): Mark said the edge hi...

> Slay icon v5.2 (2026-09-05): Mark said the edge highlights looked like a thick white border. Rim strokes reduced: top-panel rim 1.2px @0.55, lower-panel rim 1.4px @0.7, crease 2.2px with 0.85->0 fade (reference rims are ~1px, subtle). Exports refreshed: svg, 1024 png, icns, ico.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:28:43*

### Slay icon v5.4 (2026-09-05): rim highlights were c...

> Slay icon v5.4 (2026-09-05): rim highlights were centered on the panel edge so half spilled outside as a gray outline (Mark spotted it). Fix: 2px stroke clipped by the panel's own path (clipPath topClip/lowClip) = 1px visible inside the edge, opacity 0.5.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:34:05*

### Slay icon v5.1 (2026-09-05): fold gap restored per...

> Slay icon v5.1 (2026-09-05): fold gap restored per Mark's overlay screenshot: lower panel's upper-left edge ends at NOTCH (452,519), ~28px past the fold arc end (423,501), matching the reference; the twist's exposed bottom segment stays light because the shadow band now stops at the arc end. IoU 0.992, err 3.3/255. Mark compares by putting original and mine side by side and circling the spot; do the same before sending.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:25:44*

### Slay icon v3.1 (2026-09-05): Mark flagged a hard d...

> Slay icon v3.1 (2026-09-05): Mark flagged a hard dark 'stain' inside the ribbon twist. Cause: twist shading was a straight 42.5-degree gradient while the panel edge is 37.5 degrees, so the dark stop read as a wedge. Fix in gen_icon.py: shadow band = blurred black stroke (w80, blur28, op0.5) following the top panel's edge + fold arc, clipped to the twist, over a monotone 4-stop base gradient (#6b7886->#f8f9fa at 50 degrees); jointly least-squares fitted, twist err 10.9, overall err 3.3/255.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:02:44*

### Slay icon v4 (2026-09-05): Mark rejected the notch...

> Slay icon v4 (2026-09-05): Mark rejected the notch-style fold 3 times ('black eating the ribbon') and said the twist must tuck UNDER the top panel. gen_icon.py now: top panel = full polygon [TL, TIP_TR, FOLD_V(377,473) r18, BL_TOP]; twist and lower ribbon extend to FOLD_V beneath it; lower panel's upper-left edge runs straight from TIP_BL via BEND(262,603) into FOLD_V (APEX=FOLD_V) so no twist edge is exposed to the gap wedge. This deliberately deviates from the AI reference (IoU 0.95). Lesson: check the zoomed render of the fold before sending; Mark's crops point at the fold corner.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:15:20*

### Slay icon v5.6 (2026-09-05): dark hairline along t...

> Slay icon v5.6 (2026-09-05): dark hairline along the top panel's right edge was an anti-aliasing seam (twist and panel edges coincide, black drop shadow beneath showed through). Fix: twist path gets stroke=same gradient width 2.5 so it extends ~1px under the panel; shadow band also offset translate(5,6.5) into the twist so the edge isn't its darkest point.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:36:21*

### Lesson (2026-09-03): a ResizeObserver-driven SVG w...

> Lesson (2026-09-03): a ResizeObserver-driven SVG with pixel width inside a CSS grid 1fr track causes an infinite layout loop that hangs the tab (1fr = minmax(auto,1fr) grows with content). Use svg width=100% + viewBox and grid minmax(0,1fr) / min-w-0.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T20:27:03*

### docgen: base.docx sets Normal to before:0/after:0,...

> docgen: base.docx sets Normal to before:0/after:0, so EVERY vertical gap in a generated proposal comes from theme.yaml "space:" (paragraph_after, list_item_after, list_after, table_after, image_after, bold_lead_before). A blank line in the markdown produces no visible gap without it. Also added: table columns sized by content weight (table.column_widths: content) instead of equal, never narrower than the longest word in the column; page.keep_lead_with_next stops orphaned lead-ins; parser accepts Word-pasted bullets (bullet, middot, square, circle) as list markers.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:02:37*

### Slay icon v3 (2026-09-05): apps/desktop/assets/ico...

> Slay icon v3 (2026-09-05): apps/desktop/assets/icon.svg is now generated by gen_icon.py from pixel-measured geometry of Mark's reference (reference.png, 1254px AI render). Method: trace contours with OpenCV, optimize corner radii/vertices by silhouette IoU (0.991), least-squares fit of tile/panel/twist gradients, grid-search drop shadows on halo ring. Mean abs error 3.3/255 over the tile. Lesson: Mark wants pixel-level comparison overlays, not eyeballing; the fold is a rounded corner (r=35, vertex 377,473) between the top panel's right and bottom edges, and the twist shading is a 42.5-degree 6-stop linear gradient. Exports icon-1024.png + icon.icns; .ico still needs ImageMagick.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:59:41*

### Lesson: when a compose service is renamed/removed,...

> Lesson: when a compose service is renamed/removed, old containers become orphans that compose commands cannot address and they keep holding ports; remove them with docker rm -f <container>. Also stop postgres before removing its volume.

*Confidence: 0.9 | Status: active | Created: 2026-09-02T18:55:00*

### Lesson: on Mark's Mac the shadcn CLI hangs and mss...

> Lesson: on Mark's Mac the shadcn CLI hangs and mssql-tools image mcr.microsoft.com/mssql-tools18/mssql-tools does not exist; use the mssql/server image itself for sqlcmd. Airflow provider versions must not be pinned when installing under the Airflow constraints file.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T17:46:55*

### docgen house pattern for a section on ONE landscap...

> docgen house pattern for a section on ONE landscape page: wrap it with <!-- landscape --> ... <!-- portrait --> markers around the heading, chart and notes together, and keep gantt.landscape false (that option gives the chart its own landscape page and strands the heading on the previous one). Charts are rendered at the proportions of the page they go on - gantt.landscape_width_in 10.4 and landscape_height_per_task_in 0.32 vs height_per_task_in 0.40 for portrait - because a chart drawn for a portrait column is too tall for a landscape page once the heading and notes take their share of the 6.57in body height.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:46:01*

### docgen supports per-tag line height: theme.yaml to...

> docgen supports per-tag line height: theme.yaml top-level line_height: {body, h1, h2, h3, list, table} as multipliers. body and h1-h3 are written onto the STYLES (w:line with lineRule auto counts 240ths of a line, so 1.45 -> 348) so everything inheriting Normal moves together; list and table are applied per paragraph because they must differ from the Normal they inherit. A bare scalar is shorthand for {body: n}. House values body/list 1.45, table 1.30, h1 1.15, h2 1.20, h3 1.25 - raising body from 1.15 to 1.45 took the DAOL build 26 -> 28 pages.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:51:51*

### mill-tower realtime (2026-09-03): /api/events SSE ...

> mill-tower realtime (2026-09-03): /api/events SSE from one shared watcher (events.py) polling Airflow every 2s with batched requests; web useLiveUpdates hook updates TanStack cache + toasts. Lesson: per-tab pollers hitting Airflow v2 API exhausted the api-server SQLAlchemy pool (TimeoutError) and hung it; fixed by shared watcher, /dags/~/dagRuns batch endpoint, and POOL_SIZE 20 / MAX_OVERFLOW 30. Also stale uvicorn processes kept running after pkill -f 'uvicorn mill_tower'; use pkill -9 -f uvicorn.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T19:53:35*

### docgen: theme.yaml space.h1_before_on_new_page (de...

> docgen: theme.yaml space.h1_before_on_new_page (default 0) overrides Heading1 space-before when the heading starts a page - the 20pt in the style is right mid-page but wrong at the top, where it stacks on the top margin and pushes section-opening pages visibly lower than pages opening with a table. Two measurement gotchas found while tuning: (1) LibreOffice/Word SUPPRESS space-before after a hard page break, so the setting only visibly affects headings that reach a new page via a section break rather than w:pageBreakBefore - which is why 0 gives a uniform result and any positive value does not; (2) pdftotext -bbox measures GLYPHS, so a table page reads 5pt lower than a heading page purely because of table.cell_margin_twips - the table border is actually level.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:46:43*

### Lesson: WinRMOperator in airflow-providers-microso...

> Lesson: WinRMOperator in airflow-providers-microsoft-winrm (Airflow 2.10 constraints) has no expected_return_code arg; and when passing Windows paths through curl JSON use a single escaped backslash ("\\Corp\\" in shell = \Corp\).

*Confidence: 0.95 | Status: active | Created: 2026-09-02T18:39:23*

### docgen cover artwork redraw - three geometry traps...

> docgen cover artwork redraw - three geometry traps, all hit and fixed: (1) line extents must be the CONTIGUOUS run of ink along the line, not ink near the infinite line, and Hough fragments must be clustered onto one line first - deduplicating fragments by (angle, offset) leaves lines broken off short; (2) shared corners must be placed at the least-squares INTERSECTION of the lines meeting there - averaging the endpoints pulls an endpoint up to 3.75pt off its own line so the stroke overshoots the corner; join tolerance 0.03 - at 0.02 a five-way junction in image2 left one line unjoined; (3) a stroke lying on the canvas edge loses half its width to clipping, so render with a small margin and run only FREE endpoints out past the edge - extending a shared corner makes each line overshoot the other. House weight: 0.35pt, colour #BFC3C7, 1200 dpi.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:37:16*

### docgen supports table banner rows: a table row who...

> docgen supports table banner rows: a table row whose ONLY non-empty cell holds a **bold** label is merged (w:gridSpan) into a full-width shaded group heading. Requiring bold is deliberate - inferring banners from "only one cell has text" would swallow real data rows with an empty number cell. Implementation gotcha: set gridSpan AFTER capturing the raw w:tc elements, because python-docx row.cells repeats a merged cell once per spanned column, so deleting "cells after the first" through that view deletes the first cell again.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:10:16*

### Lessons (2026-09-03): Airflow 3 creates DAGs pause...

> Lessons (2026-09-03): Airflow 3 creates DAGs paused by default -> set is_paused_upon_creation=False for generated workflow DAGs. Windows Server 2022 ships PowerShell 5.1: no ConvertTo-Json -AsArray. WinRM -EncodedCommand limit ~8k chars: upload long scripts in 2000-char base64 chunks then run with -File. sp_start_job is async: poll sysjobactivity/sysjobhistory to make dependencies real.

*Confidence: 0.95 | Status: active | Created: 2026-09-02T19:23:03*

### Plane desktop app (v2.0.0) refuses Community Editi...

> Plane desktop app (v2.0.0) refuses Community Edition: error 'Your Plane instance is on v1.4.2... Upgrade to Plane Commercial v3.0.0 or later'. Our fork is Community v1.4.2 (AGPL). Commercial edition is closed-source, installed via prime-cli, and is where desktop app, workflows/approvals, SSO, epics, integrations live. Desktop app cannot be used with Slate; Slate needs its own Electron/Tauri wrapper. Observed 2026-09-04.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:30:36*

### docgen page setup is theme-driven: theme.yaml page...

> docgen page setup is theme-driven: theme.yaml page.margin_top_in / margin_bottom_in / margin_left_in / margin_right_in / header_distance_in / footer_distance_in are written onto base.docx sectPr BEFORE rendering, so every later section (landscape included) inherits them. House values 1.0/1.0/0.6/0.6 with 0.35 header+footer distance. Verify gaps by measuring the PDF with pdftotext -bbox across every page and taking the minimum, not by trusting the margin numbers - the visible gap is margin minus distance minus the header font height. Raising margins shrinks the landscape body height, so gantt.landscape_height_per_task_in had to drop 0.32 to 0.30 to keep section 8 on one page.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:51:34*

---

## Observations

*Patterns noticed, behavioral notes, and recurring themes.*

*No memories of this type.*

---

## Artifacts

*Tool outputs, files, reports, and external references.*

### Slay icon variants (2026-09-05): gen_icon.py also ...

> Slay icon variants (2026-09-05): gen_icon.py also emits logo-mark-black.svg/png and logo-mark-white.svg/png (flat silhouette, cropped viewBox 140 150 660 586, 1px same-colour stroke to hide the shared-edge AA seam) and icon-inverse.svg/png/icns/ico (light tile #fff->#d9dde2, dark ribbon gradients, softer shadows). Requested by Mark 'Version Inverse like the example' (app icon + flat glyph).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:45:33*

### Slay flat mark variants (2026-09-05): Mark said th...

> Slay flat mark variants (2026-09-05): Mark said the flat black glyph loses the fold. gen_icon.py now emits logo-mark-{black,white}-{knockout,gap,twotone}.svg/png: knockout = 7px cut along fold arc + crease; gap = 28px mask stroke around the top panel separating layers; twotone = twist at 50% opacity (lower masked so it isn't stacked). Awaiting Mark's pick.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:48:24*

### Slay app icon vector created 2026-09-05 at apps/de...

> Slay app icon vector created 2026-09-05 at apps/desktop/assets/icon.svg (1024 viewBox: dark rounded tile #282d35->#0d1014 rx 228, S mark = two light parallelogram ribbon panels point-symmetric about center with a folded twist band). icon-1024.png rendered via rsvg-convert. Reference image was Mark's generated icon; icns/ico for electron-builder not yet made.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:30:31*

### Slay desktop v0.2 (2026-09-05): apps/desktop rewri...

> Slay desktop v0.2 (2026-09-05): apps/desktop rewritten as a tabbed browser-style shell like Plane Desktop: main.js uses BaseWindow + WebContentsView per tab (class TabbedWindow), chrome/index.html tab strip (traffic-light spacer, back/forward, tabs with favicon/title/close, + button, win/linux window controls), preload.js exposes window.slay IPC bridge. Same-origin window.open -> new tab, external -> system browser. Shortcuts Cmd+T/W/N, Cmd+[ ], Cmd+Shift+[ ], Ctrl+Tab. Verified running against http://localhost (tabs open programmatically and via strip IPC; osascript keystrokes don't reach Electron without Accessibility permission). Repo HEAD da1a7ab == upstream/preview latest, tag v1.4.2 == running stack 1.4.2 (confirmed 2026-09-05).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T22:02:11*

### Slay wordmark lockup (2026-09-05): gen_icon.py emi...

> Slay wordmark lockup (2026-09-05): gen_icon.py emits logo-lockup-black/white.svg+png = gap mark + 'Slay' in Inter Display Medium (OFL; font kept at apps/desktop/assets/fonts/ with LICENSE-Inter.txt), glyphs converted to paths via fontTools with GPOS kerning and -0.02em tracking; mark height = 1.18 x cap height, vertically centred on cap height. Mark asked for both with and without wordmark (mark-only = logo-mark-*).

*Confidence: 1.0 | Status: active | Created: 2026-09-04T21:53:42*

### mill-tower demo built 2026-09-03 (commit after a07...

> mill-tower demo built 2026-09-03 (commit after a07ca49): docker-compose runs Airflow at :8081 (port 8080 is occupied by moorcheh-onprem-server, never reuse it), cron-host container (ssh :2222 demo/demo, real crontab), SQL Server 2022 amd64 with Agent jobs (:1433 sa/MillTower!2026, init via docker compose run --rm mssql-init), Windows tasks as exported XML in demo/windows (execution simulated, WinRM in prod). Importers -> JobSpec -> dags/imported/*.json -> dags/mill_tower_imported.py generates DAGs. All three sources verified running end-to-end. scripts/demo.sh brings it up.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T17:46:54*

### mill-tower runtime architecture diagram lives at d...

> mill-tower runtime architecture diagram lives at docs/architecture.html (archify, spec docs/architecture.archify.json, showcase profile passed 9/9 checks, delivered 2026-09-03). Repo has no GitHub remote so archify 'sources' evidence cannot be pinned.

*Confidence: 1.0 | Status: active | Created: 2026-09-02T20:39:34*

### Slay icon v2 (2026-09-05): apps/desktop/assets/ico...

> Slay icon v2 (2026-09-05): apps/desktop/assets/icon.svg rebuilt as a continuous ribbon S using source-image coordinates (viewBox 185 175 890 890): upper panel path with sharp top-right tip and rounded bottom-left, lower ribbon = twist band + lower panel as one path, twist shaded by a userSpaceOnUse gradient. Exports: icon-1024.png and icon.icns (iconutil). No .ico yet (ImageMagick not installed). Mark asked for it to match his reference image exactly ('Ribbon S').

*Confidence: 1.0 | Status: active | Created: 2026-09-04T20:33:47*

### Slate local self-host (2026-09-04): Plane cloned i...

> Slate local self-host (2026-09-04): Plane cloned into ~/Labs/slate (shallow, remote named 'upstream' -> makeplane/plane, commit da1a7ab 2026-09-02). Runs with prebuilt images via: cd deployments/cli/community && docker compose -p slate --env-file .env -f docker-compose.yml up -d (.env copied from variables.env with generated SECRET_KEY/LIVE_SERVER_SECRET_KEY, gitignored). Serves http://localhost (port 80): / web, /god-mode/ admin, /spaces/ public, /api/. Root docker-compose.yml builds from source and is the path for actual Slate development. Plane license is AGPL-3.0.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T09:04:49*

### Plane Commercial trial (2026-09-05): Mark had run ...

> Plane Commercial trial (2026-09-05): Mark had run 'sudo prime-cli setup' on 2026-09-04 with domain codefin.tld which left a root-owned partial install at ~/opt/plane (v3.1.4, config files only, no sudo available to Claude). Workaround: copied plane.env/docker-compose.yml/Caddyfile/.config.env to ~/plane-commercial, rewrote domain to 127.0.0.1 and INSTALL_DIR, and run with: cd ~/plane-commercial && docker compose -p plane-commercial --env-file plane.env -f docker-compose.yml up -d. Uses port 80/443, so Community stack (project slate) must be stopped first. prime-cli rejects 'localhost' as a domain; use an IP.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T19:48:35*

### Slate desktop shell created 2026-09-04 at apps/des...

> Slate desktop shell created 2026-09-04 at apps/desktop (Electron 41, npm, not yet in pnpm workspace deps): main.js loads SLATE_URL (default http://localhost), same-origin nav stays in window, external links open in browser. Run: cd apps/desktop && npm install && SLATE_URL=http://localhost npm start. Created because Plane's official desktop app refuses Community edition. Also: Docker Desktop VM died with I/O errors on 2026-09-04; fix was pkill -f com.docker.backend + pkill -x 'Docker Desktop' then open -a Docker (osascript quit alone left backend hung). Container os-compat failed to restart afterwards.

*Confidence: 1.0 | Status: active | Created: 2026-09-04T12:09:30*

---

## Errors

*Failure records, bugs, and lessons learned from mistakes.*

*No memories of this type.*

---

*End of memory export.*
