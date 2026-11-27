# Postbox crash collector

Live endpoint: https://postbox-crashes.valentin-yeo.workers.dev/v1/crashes

`GET /health` returns `ok`. `POST /v1/crashes` accepts a JSON object up to 16 KiB and stores it for 30 days. Reports use reverse-timestamp keys so reads return the newest first. `GET /v1/crashes?limit=20` requires the private `READ_TOKEN` bearer token and returns at most 50 reports.

This uses Workers and KV without paid features. Writes are public so installed apps need no embedded secret. Spam can exhaust the free quota; rejected uploads remain on disk for retry. Do not send chat contents or credentials. Panic messages reuse Postbox's existing sanitizing. Backtraces can contain build paths.

## Deploy

Use Wrangler 4 with `CLOUDFLARE_API_TOKEN` in the environment:

```sh
cd crash-worker
wrangler secret put READ_TOKEN < ~/.config/postbox-crashes-read-token
wrangler deploy
```

The read token is a random 32-byte hex value stored only in that home-directory file, mode 600, and as a Worker secret. Never print it or put it in this repository. Reuse it on redeploy; rotating it invalidates existing read access.

The configured `CRASHES` binding uses namespace `POSTBOX_CRASHES`. Its account and namespace identifiers are in `wrangler.jsonc`.

## App behavior

Rust panics write `pending-crash.json` alongside `exit-records.jsonl`, including a forced backtrace capped at 4096 UTF-8 bytes. The app attempts an immediate upload for up to two seconds. Windows SEH crashes write the exception code and address using only preallocated data and raw Win32 I/O, then continue normal Windows exception handling. Fail-fast termination and a destroyed stack can bypass the Windows filter.

On startup, a background thread uploads the pending file with a five-second request timeout. Only HTTP 2xx clears the uploaded report; a newer panic is preserved. Set `POSTBOX_CRASH_URL` to override the endpoint. This is a single pending report, not a durable queue: another crash can replace an unsent one.

## Tests

```sh
node --test src/index.test.mjs
```

The repository CI also tests panic payloads, UTF-8 truncation, Windows exception formatting, HTTP upload, and retaining reports on failure.
