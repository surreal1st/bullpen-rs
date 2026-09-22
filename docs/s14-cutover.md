# S14 — Cutover checklist (TS :4360 → rust :4380)

**Do not** repoint live `rainmade.io/bullpen/` or restart `bullpen.service` on `:4360` until every **Go** item below is checked by Josh (or linked automated check is green).

Store copy (Project Context): same content maintained under Project docs `s14-cutover.md`.

## Preconditions

- [ ] `scripts/gate.sh` green on **workstation Git Bash** (authoritative; Ubuntu CI / WSL runs are supplemental).
- [ ] `main` deployed to meridian **`bullpen-rs.service` on :4380** via `scripts/ship.sh` + `sudo bash …/install.sh` (separate from live TS).
- [ ] Parity inventory reviewed: `.scratch/bullpen-rs/INVENTORY.md` + `.scratch/bullpen-rs/reviews/PARITY-2026-09-22-RESULT.md` — no unresolved **ship-before-cutover** items (or Josh waives in writing on the parity review).

## Automated checks (repo)

Run from bullpen-rs root against **:4380** (never :4360):

```bash
export BULLPEN_URL="${BULLPEN_URL:-http://meridian:4380}"
scripts/cutover-check.sh          # unsigned gates
scripts/cutover-smoke.sh          # + signed-in API if password env set
```

On **meridian** (refresh rust DB from live TS file — read-only on live side):

```bash
sudo scripts/cutover-db-copy.sh                    # dry-run + hashes
sudo env CUTOVER_DB_COPY=1 scripts/cutover-db-copy.sh  # apply; stops bullpen-rs only
```

| Script | Result doc |
| --- | --- |
| `cutover-check.sh` | `.scratch/bullpen-rs/reviews/S14-cutover-check-RESULT.md` |
| `cutover-smoke.sh` | `.scratch/bullpen-rs/reviews/S14-cutover-smoke-RESULT.md` |
| `cutover-db-copy.sh` | dry-run output in smoke RESULT; env audit in `S14-env-verify-RESULT.md` |

`cutover-check` verifies: `/api/health`, `/api/version`, auth gates on `/api/roster` and `/api/library`, `/api/push` route behavior.

## Manual / Josh-only

| Step | Owner | Notes |
| --- | --- | --- |
| Copy production `bullpen.db` snapshot to rust data dir | Josh | `sudo scripts/cutover-db-copy.sh` (dry-run); `CUTOVER_DB_COPY=1` to apply — stops **bullpen-rs** only |
| Smoke: sign-in, send message, approval, spend read | Josh | Rendered proof, not log-only |
| iOS push on device | Josh | Out of internal finish line; optional |
| **Reverse proxy** flip `rainmade.io/bullpen/` upstream `:4360` → `:4380` | Josh | Snippet below — **check each line before apply** |
| Leave TS `bullpen.service` running | Josh | Default: 24h beside rust after proxy flip |
| Desktop `BULLPEN_URL` / launcher | Josh | `deploy/Bullpen-rs-desktop.cmd` → `:4380` |

## Reverse proxy flip (nginx — do not apply without checkbox)

Known location file on workstation/meridian tree: `projects/bullpen-gate/deploy/nginx-bullpen-location.conf` (same pattern as live TS deploy).

- [ ] Edit the server block that serves `rainmade.io` (path on meridian: site config under nginx — confirm with `grep -r bullpen /etc/nginx` before edit).
- [ ] Change upstream port **4360 → 4380** only inside the `/bullpen/` location:

```nginx
    location ^~ /bullpen/ {
        # WAS: proxy_pass http://127.0.0.1:4360/;
        proxy_pass http://127.0.0.1:4380/;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 600s;
        proxy_send_timeout 600s;
        client_max_body_size 64M;
    }
```

- [ ] `nginx -t` then reload (`systemctl reload nginx` or your host’s equivalent).
- [ ] Hit `https://rainmade.io/bullpen/api/version` — commit/build should match meridian `:4380`, not TS.

**Tailscale / direct:** rust already listens on `0.0.0.0:4380` (`deploy/bullpen-rs.service`). LAN/tailnet clients can aim at `:4380` before the public flip.

## TS decommission (after 24h stable on :4380 via public URL)

Only after proxy serves rust and Josh’s smoke is clean for a full day:

- [ ] Confirm no active users/scripts still target `http://127.0.0.1:4360` or TS-only routes missing on rust (parity review **post-cutover OK** list).
- [ ] `sudo systemctl stop bullpen.service` on meridian (TS `:4360`).
- [ ] `sudo systemctl disable bullpen.service` (optional; keeps accidental restart off — Josh checkbox).
- [ ] Document final TS tree path (`/home/bullpen/…` as deployed today) in handoff; **do not delete** TS checkout or DB without Josh’s explicit deletion approval.
- [ ] Update internal runbook links to rust-only; keep TS repo read-only for reference until archived.

## Rollback

1. Repoint proxy back to `:4360` (reverse the snippet above).
2. `sudo systemctl start bullpen.service` if stopped.
3. Rust `:4380` can stay running beside TS for debugging.

## Blockers (stop line)

- Any Git Bash gate red.
- Missing OpenRouter / TypeSafe / settings key file / `PUBLIC_URL` on meridian (see `deploy/bullpen-rs.env.example` — names only).
- DNS/proxy change without Josh checkbox above.
