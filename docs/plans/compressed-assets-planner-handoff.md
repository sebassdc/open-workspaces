# Compressed Ubuntu delivery — planner handoff
Updated: 2026-10-04 | Size: M | State: DONE, reviewed, merged and published

Owner authorized integrated compressed delivery and project-only publication. Core commit `62f73513bb5d9ffaae93addae3161f0eb524d3c1` merged as `66d1967`. [Report](compressed-assets-report.md), [source review](compressed-assets-review.md) and [brief](compressed-assets-brief.md) preserve the method, contracts and limits. Builder/reviewer panes are closed.

Zstandard level19 reduces the pristine Ubuntu image from 8,589,934,592 to 442,888,634 bytes (422.4 MiB, 94.84% reduction); complete bundle is 631,333,954 bytes (~602.1 MiB). Selected after a bounded Zstd/XZ comparison; no universal best-algorithm claim. Expanded image SHA remains `87ba61583f33e129f08776d1a4b8c1557322f146a7de0ed44581e2000f3f3f6b`.

15 focused native tests, 21 actual static CLI private-CA TLS cases and a full pristine-image TLS update passed. Full decode verified original bytes and sparse output. Public default-TLS CLI/checksum/manifests, compressed and raw HEAD routes, bounded 64KiB encoded prefix, protected dashboard and node endpoint passed. Full public or remote download timing was not measured.

Private release `data/host-verify/compressed-release-1791150224053685644/` contains accepted pins, preparation, online catalog backup, deployment/preservation, transfer-finished, public and final-status receipts. `current-progress-cli.json` selects its CLI, gateway, assets and dual-format bundle. CLI SHA `7b07c2d888022c9d1fa250962661db568a1d880aeba26dfced118efd114d8251`; compressed manifest SHA `766d83d133515ac981dc9667cf89a255d0f7cc904b750d9f225dc8135dcaf1ee`. Original raw manifest/routes remain available.

The previous raw transfer finished before activation; it was not interrupted. The owner's later activate-now reply arrived after publication. Only gateway and dedicated tunnel restarted; original Ubuntu VMM/disk, workers, agents and controller retained their identities. No Cloudflare API/DNS/Access/global SSL changes, VM actions, remote SSH or push. Remote node remained offline; this is not remote acceptance.

For future compressed downloads, upgrade the CLI with the existing HTTPS installer while the host is stopped, then use `ow host update-assets --ubuntu-dev` normally. Already downloaded Ubuntu need not be downloaded again. Old clients keep raw delivery. The unrelated unreadable-process/1Password inventory issue remains open.

Private operator instructions: `data/host-verify/production-runbook.md`, `deployment-handoff.md`, and `second-host-commands.sh`. Exact identity management uses `capacity-manage.py`; prepared cold recovery uses the current selection and refuses live runtime. Rollback retains previous selection and the owned CLI route; never stop legacy workers or restore whole catalogs/disks. Existing pending management files remain outside this task's scoped commit. No further tests or guests are needed for this completed task.
