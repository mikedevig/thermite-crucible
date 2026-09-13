# Exliatycl client (Rust)

Node agent. Binaries, certs, configs, and node state live in **`./exliatycld/`** (override with `--exliatycld` / `EXLIATYCLD_DIR`).

When the dashboard sends `first.setup.config` (or a core switch via `server.config.update`), the agent downloads **Hysteria2** or **Xray-core** for this OS/CPU from GitHub releases, writes config next to the binary, and starts it.

```
exliatycld/
  state.json          # setup_code + control token
  users.json
  certs/server.crt
  certs/server.key
  hysteria2/hysteria[.exe]
  hysteria2/config.yaml
  xray/xray[.exe]
  xray/config.json
```

## Run

```
cargo run --release -- --server http://127.0.0.1:3000 --public-host YOUR.PUBLIC.IP
```

Prints a setup PIN. Enter that PIN on the dashboard. Re-runs reuse `exliatycld/state.json`. `--force-download` re-fetches cores.

## Control contract

1. `GET /api/regticket` → print `setup_code`, keep `token`.
2. `ws(s)://SERVER/clientws?token=…` → hello, then actions.
3. `first.setup.config` → download core, write config, reply URL + `reqId`.
4. ALIVE every 2s.
5. `user.createnew` / `user.ban` / `user.kick` → ACK.
6. `server.config.update` → rewrite config, ACK.

## Multi-user auth (live, no restart)

Each VPN user gets their own credential + share URL (`vpn_users` table on the
control server; per-server "Users" tab on the dashboard). On the node side,
`user.createnew` / `user.ban` are applied through each core's **real**
management API instead of rewriting the config file and restarting:

- **Hysteria2**: `auth.type = http` in `hysteria2/config.yaml`, pointed at a
  tiny loopback HTTP server this agent runs itself
  (`src/core/hy2_auth.rs`, port `38214`), implementing exactly the
  request/response contract from [Hysteria2's docs](https://v2.hysteria.network/docs/advanced/Full-Server-Config/#authentication).
  Add/ban a user = one in-memory map update. Existing sessions of other users
  are untouched.
- **Xray**: an `api` block in `xray/config.json` (loopback, port `38216`)
  exposes Xray-core's own gRPC `HandlerService`. `src/core/xray_api.rs` calls
  `AlterInbound` with `AddUserOperation` / `RemoveUserOperation` — the same
  mechanism panels like 3x-ui/Marzban/Remnawave use. The `.proto` stubs
  under `proto/` are trimmed to just the messages we send, but are
  wire-compatible with upstream Xray-core (gRPC only cares about the
  service/method name and the field numbers on the wire).

`user.kick` (dropping an *already-connected* session, as opposed to
gating future ones) still falls back to a full restart: neither core's API
exposes a "close this open connection now" call, so it briefly affects every
user on that server (clients normally auto-reconnect right away). The
dashboard's Kick button already surfaces this caveat.

A full config rewrite + restart is still used for genuine server-level
changes (port, TLS/obfs, protocol, bandwidth caps) — those inherently need
the listening socket to come back up differently.
