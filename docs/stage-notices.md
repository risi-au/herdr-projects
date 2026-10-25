# Stage-entry notices

The board sends a message to the task's owning Coordinator when a task enters `approval` or a
persisted blocked stage (`asks`, `reviewneeds`, `plan`, `pick`, failed authentication, a stopped
limit, or a blocked dependency). This uses the actual atomic task write in
`store::tasks::update_wait`: the task carries `stageNotice.serial`, `stageNotice.stage` and
`stageNotice.entered`. Another write while it remains in that same stage is not a new entry.
Leaving and re-entering advances the serial, including when the same wall-clock millisecond is
used for both entries.

After that write commits, one detached `stage-notice` process enqueues a message in the
Coordinator conversation named by the task's own `coordinator` and `link.socket` fields.
A human-owned task routes to that session's main Coordinator. Where neither route is known,
the board does not guess a recipient. The notice contains task id, stage, entry, PR link,
head SHA and the actual merge-policy decision. `merge.by`, `requireApproval`, ownership and
autopilot holds determine whether a human approval is required. The message cannot approve
or merge anything itself.

That same process sleeps **once**, until 30 minutes after the recorded entry, then checks the
task under its lock. Only if the same entry and stage still exist does it invoke the existing
Omarchy relay `herdr-board-wake post` once. The relay's existing configuration owns the HTTP
destination. Its argument contains the task id, stage, entry, age, PR and SHA. It runs through
structured process arguments, without a shell. `HERDR_PROJECTS_WAKE_BIN` can replace its
executable for an isolated test; the normal executable is
`/home/rishi/.local/bin/herdr-board-wake`. `HERDR_PROJECTS_STAGE_BIN` likewise lets isolated
integration tests run the built CLI instead of their test harness executable.

The flags `stageNotice.pinged` and `stageNotice.alerted` are claimed **before** the respective
external action, under the task lock. This guarantees at most one attempt to enqueue a prompt
and one attempt to post an alert per entry, including simultaneous callbacks. A process crash,
unavailable coordinator or failed relay can lose a notice rather than repeat it. Restarting
the board does not reconstruct a killed deferred process. No polling loop, daemon or timer
service is installed. Leaving the stage, completion and a later entry all disarm stale jobs.
