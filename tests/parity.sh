#!/usr/bin/env bash
# Side-by-side parity harness: run read-only commands through python arr.py and
# the Rust binary, diff outputs (stdout+stderr merged) and exit codes.
PY="python3 /data/arr/arr.py"
RS="/data/arr/target/release/arr"
pass=0; fail=0
run() {
  desc="$1"; shift
  py_out=$($PY "$@" 2>&1); py_rc=$?
  rs_out=$($RS "$@" 2>&1); rs_rc=$?
  if [ "$py_out" == "$rs_out" ] && [ "$py_rc" == "$rs_rc" ]; then
    pass=$((pass+1)); echo "PASS  $desc"
  else
    fail=$((fail+1)); echo "FAIL  $desc (py=$py_rc rs=$rs_rc)"
    diff <(printf '%s\n' "$py_out") <(printf '%s\n' "$rs_out") | head -15
    echo "  ---"
  fi
}

run "help" --help
run "unknown svc" nonsense
run "unknown cmd" radarr frobnicate
run "radarr status one" radarr status "Iron Man 2"
run "radarr status miss" radarr status "zzz-no-such-movie"
run "sonarr status one" sonarr status "Silicon Valley"
run "sonarr seasons" sonarr seasons "Silicon Valley"
run "radarr info" radarr info "Iron Man 2"
run "sonarr info ambiguous" sonarr info "the"
run "radarr get" radarr get "Iron Man 2"
run "radarr files" radarr files "Iron Man 2"
run "sonarr episodes" sonarr episodes "Silicon Valley" --season 1
run "radarr queue" radarr queue
run "sonarr queue" sonarr queue
run "radarr history" radarr history "Iron Man 2"
run "sonarr wanted" sonarr wanted
run "radarr lookup" radarr lookup "Reversal of Fortune"
run "sonarr parse" sonarr parse "Show.S01E01.1080p.WEB.x264-GRP"
run "queue overview" queue
run "sab queue" sab queue
run "sab status" sab status
run "seerr requests" seerr requests
run "bazarr status" bazarr status
run "jellyfin has" jellyfin has "Iron Man 2"
run "radarr availability" radarr availability "Iron Man 2"
run "radarr delete dry" radarr delete "Iron Man 2"
run "sonarr coverage" sonarr coverage "Silicon Valley"
run "radarr audit" radarr audit "Iron Man 2"
run "sonarr-anime status" sonarr-anime status "Link Click"

echo; echo "== $pass pass, $fail fail =="
#!/usr/bin/env bash
PY="python3 /data/arr/arr.py"
RS="/data/arr/target/release/arr"
pass=0; fail=0
run() {
  desc="$1"; shift
  py_out=$($PY "$@" 2>&1); py_rc=$?
  rs_out=$($RS "$@" 2>&1); rs_rc=$?
  if [ "$py_out" == "$rs_out" ] && [ "$py_rc" == "$rs_rc" ]; then
    pass=$((pass+1)); echo "PASS  $desc (rc=$py_rc)"
  else
    fail=$((fail+1)); echo "FAIL  $desc (py=$py_rc rs=$rs_rc)"
    diff <(printf '%s\n' "$py_out") <(printf '%s\n' "$rs_out") | head -12
    echo "  ---"
  fi
}
run "tracks movie" radarr tracks "Iron Man 2"
run "episodes json" sonarr episodes "Silicon Valley" --season 1 --json
run "watch ready" sonarr watch "Silicon Valley" --quiet
run "watch verify" radarr watch "Iron Man 2" --verify-subs eng --quiet
run "seerr unfulfilled" seerr unfulfilled
run "seerr request one" seerr request 238
run "bazarr wanted" bazarr wanted
run "sab history" sab history
run "jf unwatched" jellyfin unwatched
run "sonarr stuck dry" sonarr stuck
run "sonarr tag show" sonarr tag "Silicon Valley"
run "sonarr raw" sonarr raw /health
run "info by id" radarr info 1
run "sonarr coverage tracks" sonarr coverage "Silicon Valley" --tracks
echo; echo "== $pass pass, $fail fail =="
