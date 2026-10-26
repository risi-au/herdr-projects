# Stage-entry notices

The board sends a message to the task's owning Coordinator when a task enters `approval` or a
persisted blocked stage (`asks`, `reviewneeds`, `plan`, `pick`, failed authentication, a stopped
limit, or a blocked dependency). This uses the actual atomic task write in
`store::tasks::update_wait`: the task carries `stageNotice.serial`, `stageNotice.stage` and
`stageNotice.entered`. Another write while it remains in that same stage is not a new entry.
For blocked stages recorded by a flag, clearing that flag disarms the existing entry even if
the stage name is unchanged; setting it again creates a new entry. Leaving and re-entering
also advances the serial, including within the same wall-clock millisecond.

After that write commits, one detached `stage-notice` process enqueues a message in the
Coordinator conversation named by the task's own `coordinator` and `link.socket` fields.
A human-owned task routes to that session's main Coordinator. Where neither route is known,
the board does not guess a recipient. The notice contains task id, stage, entry, PR link,
head SHA and an approval classification from the current task record. For `approval`, it says
`NORMAL` only when the existing automatic merge policy and all its current green gates pass;
otherwise it says `UNKNOWN` with a reason (such as `requireApproval`, a blocked merge, a
human-held policy, or incomplete review/check evidence). Other blocked stages are `UNKNOWN`.
The task does not record a named human approval authority: neither `owner` nor `merge.by=you`
identifies Rishi as the approver. The message cannot approve or merge anything itself.

That same process sleeps **once**, until 30 minutes after the recorded entry, then checks the
task under its lock. Only if the same entry is still active and the qualifying blocked stage
still applies does it resolve the Omarchy recipient with
`/home/rishi/.local/bin/herdr-stuck-alert --who task:<id> --task <id>` (override:
`HERDR_PROJECTS_ALERT_ROUTE_BIN`). A successful resolution returns `to=<owner> why=<reason>`;
an unavailable or invalid route sends no alert. The board then invokes
`/home/rishi/.local/bin/ask-grokbot <owner> <detail>` (override:
`HERDR_PROJECTS_ALERT_BIN`) to perform **one** configured HTTP POST. The detail includes task
id, stage, entry, age, PR, SHA and the approval classification/reason. Transport supplies the
configured endpoint and key; no key goes in process arguments. `GROKBOT_NO_FALLBACK=1` and
`GROKBOT_RETRY_DELAYS=" "` disable fallback and internal retries. Calls use structured
process arguments without a shell. `herdr-board-wake post` writes local board status and is
not this Omarchy alert transport. `HERDR_PROJECTS_STAGE_BIN` allows isolated integration tests
to run the built CLI instead of the test harness executable. Live owner-to-Omarchy delivery
proof remains deferred until installation is separately approved and confirmed.

The flags `stageNotice.pinged` and `stageNotice.alerted` are claimed **before** the respective
external action, under the task lock. This guarantees at most one attempt to enqueue a prompt
and one resolved-recipient POST attempt per entry, including simultaneous callbacks. A process
crash, unavailable coordinator, unresolved owner or failed POST can lose a notice rather than
repeat it. Restarting
the board does not reconstruct a killed deferred process. No polling loop, daemon or timer
service is installed. Leaving the stage, completion and a later entry all disarm stale jobs.
