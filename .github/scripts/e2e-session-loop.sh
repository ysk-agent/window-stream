#!/usr/bin/env bash
# e2e-session loop (see .github/workflows/e2e-session.yml).
#
# Session drive design — an adaptation of the layer-3 session
# server design in ysk-agent/android-ci
# (docs/session-server-design.md, commit a48916f) to a repo where
# the tailnet route is not available: joining the tailnet would
# require adding a Tailscale auth-key secret to THIS repo, which
# was not approved. Instead the session is driven over git:
#
#   * At start the loop creates a per-session branch
#     `session-bus-<run id>` with `jobs/` and `results/`.
#   * The driver submits a job by pushing `jobs/<id>-<name>.job`
#     (a bash script) to that branch; the loop polls the branch
#     every ${POLL_SECS}s, runs new jobs SERIALLY in the workspace,
#     and pushes `results/<id>.log` + `results/<id>.state`
#     (state/exit/duration) back. Job files and results are the
#     async job API of the design, with files in place of HTTP:
#     POST /jobs = push a .job file, GET /jobs/{id} = read .state,
#     GET /jobs/{id}/log = read .log, POST /shutdown = push a
#     SHUTDOWN file.
#   * Lifetime is activity-based, as in the design: a job first
#     seen, a job running, and a job finishing all reset the idle
#     clock; the session never dies mid-job. It ends when
#     IDLE_MINUTES pass with no activity, when SHUTDOWN appears,
#     or at the Actions job cap (timeout-minutes on the job).
#
# Security positioning: there is no inbound network surface at
# all — no listener, no tunnel, no token. Drive authority is
# exactly "write access to this repository", the same authority
# that can dispatch the workflow or push code CI will run. Job
# definitions and results are readable by anyone who can read
# this (public) repo, so jobs must not print secrets; none are
# present in the environment beyond the workflow's own
# GITHUB_TOKEN, which is scoped to this repo and dies with the
# job. Result pushes use GITHUB_TOKEN, so they never trigger
# other workflows; driver pushes use "[skip ci]" for the same
# reason.
set -u

IDLE_MINUTES="${IDLE_MINUTES:-60}"
POLL_SECS="${POLL_SECS:-15}"
BUS_BRANCH="session-bus-${GITHUB_RUN_ID}"
REPO_URL="https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY}.git"
export BUS_DIR="${RUNNER_TEMP}/session-bus"
export SESSION_OUT="${GITHUB_WORKSPACE}/session-out"
mkdir -p "$SESSION_OUT"

echo "session: run=${GITHUB_RUN_ID} bus=${BUS_BRANCH} idle=${IDLE_MINUTES}m poll=${POLL_SECS}s"

# --- create the bus branch (orphan: README + jobs/ + results/) ---
git clone --quiet --depth 1 "$REPO_URL" "$BUS_DIR"
cd "$BUS_DIR"
git config user.email "e2e-session@users.noreply.github.com"
git config user.name "e2e-session"
git checkout --quiet --orphan "$BUS_BRANCH"
git rm -rf . >/dev/null 2>&1 || true
mkdir -p jobs results
echo "# e2e session bus ${GITHUB_RUN_ID} (branch deleted when the session is done)" >README.md
date -u +%FT%TZ >results/SESSION_START
git add -A
git commit --quiet -m "session start ${GITHUB_RUN_ID} [skip ci]"
git push --quiet -u origin "$BUS_BRANCH"
cd "$GITHUB_WORKSPACE"
echo "session: bus branch live: ${BUS_BRANCH}"

bus_push() {
  # Commit everything pending under the bus clone (results written
  # by the loop or by a job via $BUS_DIR) and push, rebasing onto
  # driver commits (which only touch jobs/) when needed.
  cd "$BUS_DIR" || return 0
  git add -A >/dev/null 2>&1
  if git commit --quiet -m "$1 [skip ci]" >/dev/null 2>&1; then
    for _ in 1 2 3 4 5; do
      if git push --quiet origin "$BUS_BRANCH" 2>/dev/null; then break; fi
      git fetch --quiet origin "+refs/heads/${BUS_BRANCH}:refs/remotes/origin/${BUS_BRANCH}" || break
      git rebase --quiet "origin/${BUS_BRANCH}" || git rebase --abort
      sleep 3
    done
  fi
  cd "$GITHUB_WORKSPACE"
}

bus_fetch() {
  # The bus clone is a --depth 1 single-branch clone, whose default
  # fetch refspec covers only the default branch: a plain
  # `git fetch origin <bus branch>` lands in FETCH_HEAD and never
  # updates refs/remotes/origin/<bus branch>, so every read of
  # origin/<bus branch> would silently see nothing. Fetch with an
  # explicit refspec instead. (Verified the hard way: the first
  # Linux probe session never saw its job and idled out.)
  git -C "$BUS_DIR" fetch --quiet origin "+refs/heads/${BUS_BRANCH}:refs/remotes/origin/${BUS_BRANCH}" 2>/dev/null || true
}

shutdown_requested() {
  git -C "$BUS_DIR" cat-file -e "origin/${BUS_BRANCH}:SHUTDOWN" 2>/dev/null
}

list_jobs() {
  git -C "$BUS_DIR" ls-tree --name-only "origin/${BUS_BRANCH}" jobs/ 2>/dev/null |
    sed -n 's|^jobs/||p' | grep '\.job$' | sort
}

last_activity=$(date +%s)
end_reason="idle"
declare -A seen done_jobs

while :; do
  bus_fetch
  if shutdown_requested; then
    end_reason="shutdown"
    break
  fi
  now=$(date +%s)
  if [ $((now - last_activity)) -ge $((IDLE_MINUTES * 60)) ]; then
    end_reason="idle"
    break
  fi
  # Find the first job (in name order) not yet done: serial queue.
  next_job=""
  while IFS= read -r name; do
    [ -z "$name" ] && continue
    id="${name%.job}"
    if [ -z "${done_jobs[$id]:-}" ]; then
      next_job="$name"
      break
    fi
  done <<EOF
$(list_jobs)
EOF
  if [ -z "$next_job" ]; then
    sleep "$POLL_SECS"
    continue
  fi
  id="${next_job%.job}"
  if [ -z "${seen[$id]:-}" ]; then
    seen[$id]=1
    last_activity=$(date +%s) # a submission is activity
  fi
  echo "session: running job ${id}"
  printf 'state=running\nsince=%s\n' "$(date -u +%FT%TZ)" >"$BUS_DIR/results/${id}.state"
  bus_push "job ${id} running"
  git -C "$BUS_DIR" show "origin/${BUS_BRANCH}:jobs/${next_job}" >"${RUNNER_TEMP}/job.sh"
  start_s=$(date +%s)
  (cd "$GITHUB_WORKSPACE" && bash "${RUNNER_TEMP}/job.sh") >"$BUS_DIR/results/${id}.log" 2>&1
  rc=$?
  dur=$(( $(date +%s) - start_s ))
  printf 'state=%s\nexit=%d\nduration_s=%d\nfinished=%s\n' \
    "$([ "$rc" -eq 0 ] && echo passed || echo failed)" "$rc" "$dur" "$(date -u +%FT%TZ)" \
    >"$BUS_DIR/results/${id}.state"
  done_jobs[$id]=1
  last_activity=$(date +%s) # a completion is activity
  echo "session: job ${id} finished rc=${rc} in ${dur}s"
  bus_push "job ${id} done"
done

printf 'reason=%s\nended=%s\n' "$end_reason" "$(date -u +%FT%TZ)" >"$BUS_DIR/results/SESSION_END"
bus_push "session end (${end_reason})"
echo "session: ended (${end_reason})"
