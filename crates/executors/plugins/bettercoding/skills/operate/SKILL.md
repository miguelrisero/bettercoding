---
name: operate
description: Use BetterCoding MCP to post and close owner Inputs, run or restart workspace sessions, share reports and files, or manage BetterCoding issues.
---

# Operate BetterCoding

Use only the BetterCoding tools exposed in the current connection. Call
`get_context` when available to find the current workspace, issue, project, and
orchestrator session. An orchestrator-scoped connection can infer its workspace
and restrict access to other workspaces. Resolve IDs with the list and read
tools before making changes.

## Inputs

Use `create_input` with the workspace ID when required. Start with
`expected_sequence: 0`. Give each new Input a distinct `external_key` in that
workspace and each write a distinct `request_id`. Reuse the request ID and
identical arguments when retrying a write after an uncertain response.

Set `priority` to `P0` through `P4`. If omitted, BetterCoding uses a leading
`P<n> · ` title prefix or defaults to `P2`. Put the title, self-contained
HTML, and answer contract inside `content`. Use exactly one
`<meta name="output-kind" content="result">` or
`<meta name="output-kind" content="decision">` in the HTML.

- **Result** reports an outcome. Always include the Seen choice,
  `{"id":"seen","label":"Seen"}`, even if other choices are present. The owner
  can select Seen and submit without typing.
- **Decision** asks the owner to choose an action. Give each option a stable,
  distinct `id` and a `label` that matches the option in the HTML. Describe
  what happens if the owner defers when you offer that option.
- The owner can type a reply with or without a choice. Set
  `allow_no_answer_yet` when the owner can explicitly defer. A deferral never
  authorizes an action. Keep secrets out of Inputs.

Use this example to build the result arguments for `create_input`.

```json
{
  "external_key": "checkout-proof-ready",
  "request_id": "checkout-proof-ready-create",
  "expected_sequence": 0,
  "priority": "P2",
  "content": {
    "title": "Checkout verification passed",
    "html": "<meta name=\"output-kind\" content=\"result\"><meta name=\"owner-class\" content=\"owner-report\"><p>Checkout passed in the test environment.</p>",
    "contract": {
      "choices": [{"id": "seen", "label": "Seen"}],
      "allow_no_answer_yet": false
    }
  }
}
```

If the environment has chief-of-chiefs Input hooks, also include the
`owner-class` meta tag. Use `owner-report` for results. Use the applicable
class for decisions from `product`, `billing`, `security-policy`,
`architecture`, or `owner-credential`. The tags classify the Input and do
not grant approval.

1. Save `input_id`, `sequence`, and `owner_url` from the create receipt.
2. Use `get_input` with `input_id` to read `input.sequence`, the version,
   answers, and the follow-up thread. Set `include_html: true` to read HTML.
3. Use `get_input_events` with the workspace and optional `input_id` to read
   the append-only history. Process events after the saved `cursor`. Save any
   pending work before persisting `next_cursor`.
4. Verify the applicable `answer_submitted` event has
   `actor_kind: "owner_ui"` and `payload.no_answer_yet: false` before acting.
   Read `payload.answer.choice_id` or `payload.answer.text`. A
   `follow_up_posted` event carries a message in `payload.text` and leaves
   the Input open.
5. Leave an Input open when the owner answers with
   `payload.disposition: "keep_open"`. The owner can close it from the card.
   For a legacy answer, close the Input after handling the answer if its
   status is still `answered`. Call `set_input_status` with `input_id`,
   `status: "closed"`, a new `request_id`, and `expected_sequence` from the
   latest `get_input`. Supply a short `reason`. Check the returned status.
   On a sequence conflict, reread and check the state before retrying.
6. Never call `update_input`. To replace content, close the old Input with a
   reason that identifies its replacement, then create a new Input with a
   fresh `external_key` and `request_id`.

## Workspaces and sessions

- Use `list_workspaces` with `archived: false` to find active workspaces. Use
  `get_workspace` and `read_workspace_screen` with `workspace_id` to verify
  app status and inspect the pane. A null screen means no pane is running.
- Use `list_repos` for repository IDs. Call `start_workspace` with `name`,
  `repositories: [{"repo_id":"…","branch":"main"}]`, `executor`, and the
  opening `prompt`. Pass `issue_id`, `model_id`, or `reasoning_id` as
  needed. Check `agent_launched` and read the workspace screen.
- Use `list_sessions` with `workspace_id` to resolve BetterCoding session IDs.
  Use `create_session` with `workspace_id`, `name`, and an explicit
  `executor`. Pass `CLAUDE_CODE` when Claude plugins or hooks are required.
- Use `update_session` with `session_id` and any of `name`, `cli_model_id`,
  or `cli_reasoning_id`. Check the returned values. Model and reasoning
  changes apply at the next launch. Select the executor at creation.
- Use `run_session_prompt` with `session_id` and `prompt`. For `started`,
  poll `get_execution` with `execution_id`. For `routed_to_cli`, check the
  delivery receipt and pane. For `queued` or a conflict, inspect current
  state before resubmitting. A busy workspace can leave another session
  queued. Use `start_workspace` when the work needs its own live pane.
- Use `launch_workspace_agent` with `workspace_id` to start a stopped agent
  or resume after unarchiving. Inspect the pane if `launched` is false.
- Use `send_workspace_message` with `workspace_id` and `text` to send an
  authorized prompt to the running CLI. Check the delivery response. The
  tool types into the prompt as the user. Use the native peer channel for
  agent coordination and the owner UI for owner answers.

### Restart an existing session

Record the session, pane, work in progress, and recovery steps before a
restart. Verify the target process and pane. Preserve the workspace, repository
directories, and conversation. A session restart must never delete workspace
directories. Leave the BetterCoding server and workspace archival state alone.

- Call `launch_workspace_agent` for the same workspace if its agent has
  stopped. Verify the resumed session and screen.
- For an ordinary Claude lane, ask the lane to finish or checkpoint background
  work and report ready. Verify the correct pane. Press Escape, Ctrl+C,
  Ctrl+U, then enter `/restart`. Confirm the same conversation and the
  expected plugin version. Restore stopped tasks from the checkpoint.
- Use the configured restart operator for the CoC. The
  operator sends Ctrl+C twice, handles the exit dialog, verifies the old
  process has exited, then sends Ctrl+D. For a chief, the verified sequence
  is Ctrl+C twice, then Ctrl+D. If an exit dialog appears, choose option 1
  after checkpointing the stopped tasks. If the agent exits to a shell, exit
  that verified pane and call `launch_workspace_agent`. If the pane has
  already closed, call the tool directly. Use BetterCoding's launch to keep
  its original settings and MCP flags. Verify the new process, conversation,
  and plugin, then restore tasks from the checkpoint.

## Reports, issues, and files

- Use `list_report_tabs` for current tabs. Use `set_report_tab` with a stable
  `tab_id`, `title`, `columns`, and rectangular `rows`. Cells have
  `text` and can have a `tone` or HTTP(S) `url`. Pass
  `expected_updated_at` to protect an update or `delete_report_tab` from
  concurrent changes. A report tab communicates status.
- Use `list_issues` and `get_issue` before editing. Use `create_issue` in
  the intended project, `update_issue` for title, description, or status, and
  `link_workspace_issue` to associate the workspace if needed.
- Use `file_link` with `path` for a file inside the workspace. A relative
  path uses the agent's current directory in its scoped workspace and the
  workspace root for another workspace. Share the returned `url`. Keep HTML
  previews self-contained and link other files with their own URLs.

## Known pitfalls

- Add the output-kind meta tag if it is missing.
- Include Seen to prevent an empty result contract and allow acknowledgment without typing.
- Replace a generic decision choice with a distinct ID and matching label for each option.
- Read `input_id` from the create receipt. `event.id` identifies the event.
- Close the old Input and post a new one when its content becomes stale.
- Pass `executor: "CLAUDE_CODE"` at session creation to load Claude hooks.
- Check the pane when a prompt queues. Use a fresh workspace for independent work.
- Pass `text` to `send_workspace_message`.
- Check the live process, transcript activity, or native reply when `ListAgents` shows `shell` or omits a peer.
- Relaunch through BetterCoding to keep the settings. Keep shared state outside disposable workspaces.
