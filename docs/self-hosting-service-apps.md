# Self-hosting: one service app per product

Status: **approved by Sean, 2026-11-20**, all five decisions as written. Being built (Part 2).

Sean's rule (2026-11-20): anything self-hosted ships with one service app of its own. It runs the
product's helper tooling and its updates. Each product updates itself, never another product.
Every updater downloads a new release ahead of time and checks it. In the product's own
Settings, the person picks between updating automatically and accepting each update by hand.

Warpcore goes first, then Synapse. The Vale gateway and the Lockdown server follow the same
pattern later; this doc doesn't change them.

## 1. What one service app is

**One container per product** (Warpcore's is `warpcore-server`). It holds:

- **The product's updater:** the shared server engine (section 2) and the product's own steps
  around it.
- **The product's helper tooling:** for Warpcore, the database backups, the sandbox self-test,
  and setting up and checking the other containers.
- **A small API:** for the product's own Settings → Updates, and a status-only view for other
  apps to read (section 4).

It runs the product's other containers. It never runs another product's containers.

It reaches Docker only through **its own socket proxy**. Only the proxy holds the Docker socket.
It sits on a Docker network that has just the two of them (`internal: true`, no route out), and
its port is never published. The proxy allows only container, image and network calls. On top of
that, the engine acts only on containers labelled `org.snds.update.app=<product>`. Anything
that can create containers is as powerful as root on the server, so that label rule lives in the
engine's code and is tested, not left to configuration.

### Warpcore's containers, one by one

| Container | Decision | Why |
|---|---|---|
| `web` | **Separate, run and updated by the service app** | Published to the tunnel. Unchanged. |
| `api` | **Separate, run and updated** | Holds the sign-ins and the database password. Merging it would give the code that handles web requests a path to Docker. |
| `sandbox` | **Separate, run and updated. Its boundary is unchanged.** | Runs untrusted code: `/dev/kvm`, every capability dropped, read-only root, on `sandboxes` with only the API (and the gateway). Nothing about it moves. |
| `artifacts` | **Separate, run and updated. CI starts publishing its image.** | Keeps its own hostname and network, with no route to the API. Today it is built on the server, so it can't be updated by digest. Once published, it becomes `server-artifacts` on the channel, and is optional (on only when its hostname exists). |
| `db` (Postgres) | **Separate and run, never updated by the engine** | Keeps its own container and data. A major Postgres version needs a dump and a restore, so a new `db` image is always a by-hand update. |
| `backup` | **Merges into the service app** | It is helper tooling: the nightly `pg_dump`, the backup before each update, and the list Settings shows. The service app runs the same `backup.sh` schedule against `db`, read-only, into the same `backups/` folder. |
| app-updates `host-helper` (D12) | **Warpcore stops using it; its role merges into the service app** | Its role for Warpcore, a backup before, waiting for tasks, recreating the containers and the self-test after, becomes the engine plus Warpcore's steps. Warpcore's install of it (`deploy/host-helper/`) moves to an archive folder, not deleted. The crate itself stays in this repo, because the Lockdown server and the Vale gateway still use it until they get service apps of their own. It isn't running on Sean's server today. |
| Synapse relay's update helper | **Its Warpcore role moves into the service app** | It stays in the relay, for Synapse only (section 4). It is switched off on Sean's server today (`SY_RELAY_UPDATES=0`). |
| Gateway stack: LiteLLM, `provider-admin`, `ollama-repair`, `desktop-proxy` | **Stays outside, for now** | Its own compose project, keys and config, all changed by hand (`byHand`) today. The service app only checks it can reach `gateway:4000` and shows that in Settings. It could become a set of run and updated services later, as a separate step. |
| `cloudflared` | **Stays outside** | One tunnel serves every app on the server (`Unraid-Cloudflared-Tunnel`), so it belongs to none of them. |

**What doesn't weaken:** `sandbox` keeps every limit it has, and the engine copies a container's
settings exactly, changing only the image and version label. `artifacts` keeps its own network.
Postgres keeps its own container, and backups stay read-only on it. Only the proxy holds the
socket, and the proxy is private to the service app. The one new power is that the service app
can create Warpcore's containers on first install (section 6). It does that from a fixed setup
built into its signed image, never from anything a request sends.

## 2. One shared server update engine

A new library crate in this repo, **`server-engine/`** (`snds-server-engine`), ported from
Synapse's `relay/src/updates/` as it is at `main`, with #233 folded in:

- **The signed channel:** a Tauri-style `latest.json`. Every service's signature must verify with
  the product's key, which is built into the service app's image, and one bad signature refuses
  the whole release. Manifests come only from GitHub's release hosts, over https, with every
  redirect checked.
- **Digest-only pulls:** `name@sha256:…`, no tag, refused before Docker is called.
- **The swap:** all images present before the first swap. Each container is rebuilt from the old
  one's own settings with only the image and version label changed. The old one is kept, stopped,
  as `<name>-previous`.
- **Health and rollback:** each new container must be healthy within 2 minutes (or keep running
  for 10 s if it has no health check). If any fails, every service goes back by itself.
  **Roll back** on request does the same. One earlier version is kept.
- **Registry sign-in** (#233): a read-only token, sent as `X-Registry-Auth` only on a pull, and
  only to the registry it is for (`ghcr.io`), never to a look-alike host. It never prints.
- **New, the product's steps:** a `Hooks` trait the engine calls around an update:
  `before()` (may refuse, and nothing changes), `ready_now()` (is this a safe moment?), and
  `after()` (reported, and it may ask for a rollback).

Warpcore's hooks, in `warpcore-server`:

| Hook | What it does |
|---|---|
| `ready_now` | Asks the API which tasks hold a VM (paused ones too). Any → wait, and name them. |
| `before` | Asks the API to stop new tasks starting, then takes a backup (`backup.sh once`). A failed backup → nothing changes. Records the backup's name with the update. |
| `after` | Runs the sandbox self-test in a one-off container copied from the new `sandbox` (only the command is set, to the fixed `node dist/selftest.js`; removed after). A failure **rolls back**, which today's helper doesn't do. Then lets tasks start again. |

## 3. The update flow (the same as the desktop kit)

1. **Check** at start and every 6 hours (a **Check automatically** switch, on by default), and on
   **Check for updates**.
2. **Download ahead of time.** When a newer signed release appears, the service app pulls every
   image by digest and checks each image's digest against the release. Running containers aren't
   touched. Settings then says **Update ready · {version}**, *downloaded*.
3. **The choice, in the product's own Settings → Updates:**
   - **Update automatically**, either *within a time window* (for example 03:00–05:00, the
     server's time zone) or *when nothing would be lost* (no task holding a VM, the same rule as
     `ready_now`).
   - **Accept each update by hand:** **Restart to update** installs the downloaded release now.
   - **Roll back**, whenever a previous version is kept.
   - **The backup taken before each update** shows next to it in the list of recent updates.
4. **`byHand` releases are never automatic.** Settings shows the step and the command behind
   **Show the command**, as it does today. A new `db` image or a change to the containers' setup
   (a new mount, port, device or setting) is always by hand.

The `byHand` flag must not be something an attacker can flip, so the channel signs it (section 5).
If its signature is missing or wrong, the release is treated as by hand, never as automatic.

## 4. Who updates what

- **Warpcore's service app** updates Warpcore's services and itself. Nothing else.
- **Synapse's relay is Synapse's service app.** It updates itself and Synapse's server parts. Its
  catalog drops Warpcore. The Warpcore key it was given in #228 (`relay/keys/warpcore-server.pub`)
  moves into `warpcore-server`'s image and leaves the relay.
- **Its helper stops managing other products' containers.** It keeps acting only on
  `org.snds.update.app=synapse`.
- **Synapse's Devices → Home server view is a window onto each service app.** Each service app
  serves a **status-only** endpoint (`GET /v1/status`: version, state, offered, previous, the last
  backup, health). The relay reads it over a shared internal Docker network, `snds-status`, that
  carries nothing else, and passes it to the Mac. The view shows it with **Open Warpcore's
  Settings**. Accepting, rolling back and the auto/manual choice happen in Warpcore. The relay
  holds no Docker access for other products and can't change them.
- **X15e, re-scoped:** X15e passes when the shared engine passes its steps through Warpcore's
  service app (old to new, ahead-of-time download, manual, rollback, automatic, the two bad
  releases, digest-only pulls), and Synapse's Home server view shows Warpcore's status from it.
  The offline tests move with the engine (`server-engine/tests/x15e.rs`). Synapse's X15e entry
  points at them and at the real-server run.

### Updating itself

A service app can't swap the container it runs in. So it **hands over**: it starts the new image
as `<name>-next`, with its own settings. The new one checks itself and reaches the proxy. Only
then does the old one stop and rename itself `<name>-previous`, and the new one take the name.
If the new one isn't healthy within 2 minutes, the old one (still running) removes it. It
updates itself last, after the product's services are healthy, and it can update alone. This is
the newest part of the design, so it gets its own tests. X15e doesn't need it.

## 5. Releases

- **The `warpcore-server` channel keeps its format:** a Tauri-style `latest.json`, one
  `server-<service>` entry per image, each signed over its `sy-server-release/1` statement.
  New entries: `server-artifacts` (optional) and `server-warpcore-server` (the service app
  itself).
- **Added, signed:** top-level `byHand` and `byHandAt`, and a `signature` over a second
  statement, so the flags can't be changed:

  ```
  sy-server-release-meta/1
  app=warpcore
  version=1.1.0
  by_hand=0
  by_hand_at=1.0.4
  ```

  An older reader ignores these fields. The engine treats a release without a valid one as by hand.
- **`warpcore-main` is retired as an update source.** The API stops reading it and the service app
  reads only `warpcore-server`. CI keeps publishing `warpcore-main` for one release, so a server
  that hasn't moved yet still sees updates, then stops. The open page's **New version ready ·
  Reload** never used `warpcore-main`: it compares `/build.json` with the build it loaded, and
  doesn't change.
- **The same signing:** CI signs with `WARPCORE_SERVER_KEY`, which lives in the GitHub secret and
  1Password only. The servers hold just the public half.

## 6. Install

**One compose file and one Unraid template per product.** A newcomer installs the service app
and it brings up the rest.

- **The compose file** (`deploy/compose.yaml` in the product's repo) has just two services: the
  socket proxy and the service app, and the private network between them.
- **The Unraid template** (`warpcore-server.xml`) asks for:
  - Cloudflare Access team and audience.
  - The owners' sign-in emails.
  - The data folder.
  - The registry user and token (masked).
  - The KVM group.
  - Optionally, the artifacts hostname.

  A template can only start one container, so the proxy comes as a second, fixed template
  (`warpcore-docker-proxy.xml`, no settings to fill in). The service app's first screen says if
  it's missing.
- **First start:** the service app creates Warpcore's networks (with today's names,
  `warpcore_default` and `warpcore_sandboxes`, so the gateway stack still joins) and the
  containers, from a setup built into its image. It makes the secrets a newcomer would otherwise
  type (database password, sandbox token, sources key) and keeps them in its data folder, mode 600,
  never shown.
- **Release keys and registry tokens are never on the server's disk in plain sight:** the release
  key never leaves CI and 1Password. The registry token lives only in the template's masked field.

### Moving Sean's current server over

1. **Back up first:** `backup.sh once`, and note the backup's name.
2. **Check there is only one updater:** there's no host-helper container, and the relay's helper
   is off. If either were on, stop it first.
3. **Install the proxy and service app** from their templates, in **adopt** mode. Adopt mode reads
   the existing `deploy/.env` (same database, same secrets) and takes over the running `warpcore-*`
   containers and networks without recreating them. It adds the labels the engine needs on the
   next update. The `postgres` folder and backups don't move.
4. **Check:**
   - The site works through the tunnel.
   - The sandbox self-test passes.
   - The gateway is reached.
   - Settings → Updates shows the service app.
5. **Retire the old path:**
   - The checkout's compose file stays for reference, marked "not the install any more".
   - The host-helper install is archived.
   - The gateway stack is unchanged.

To undo the move: stop the service app and proxy, then run the old command
(`docker compose -f deploy/compose.yaml up -d`) in the checkout. The containers and data are the
same ones.

## 7. Build order (Part 2, after the OK)

1. **snds/app-updates:**
   - `server-engine/`, ported with its tests (X15e, LS18 refusals, digest-only, registry sign-in
     never sent elsewhere).
   - New tests for the ahead-of-time download, auto against manual, the signed `byHand`, and the
     hooks.
   - The service-app shell: the API, settings, the 6-hour timer, status.
   - `host-helper/` stays, for Lockdown and Vale; Warpcore's install of it is archived in snds/warpcore.
2. **snds/warpcore:**
   - `apps/server` (`warpcore-server`): the hooks, adopt mode, setup on first start, the merged
     backups.
   - The API's Settings → Updates talks to it, and gets the auto/manual choice, *downloaded*,
     **Restart to update** and **Roll back**.
   - The new compose file and the two templates.
   - CI publishes `warpcore-server` and `warpcore-artifacts`, and signs the meta statement.
3. **snds/synapse:**
   - #233 is folded into the engine, not merged as it is (#228 is merged already).
   - The relay drops Warpcore from its catalog, uses the shared engine for itself, and reads
     `snds-status`.
   - The Home server view shows service apps.
   - `docs/model/server-updates.md`, Stage L's X15e wording and the templates are updated.
4. **Every PR:** checks green before merge. Browser tests run in CI only.

## Decisions for Sean

1. **The shape above:** one `warpcore-server` per product; `backup` and the host-helper merge
   into it; web, api, sandbox, artifacts and db stay separate and are run by it; the gateway and
   cloudflared stay outside.
2. **Two Unraid templates** (service app plus a fixed proxy), because a template is one
   container. The other way is one container with the proxy inside it as a separate user. That
   is simpler to install, but the proxy is then no longer a container of its own.
3. **A failed sandbox self-test rolls back**, where today's helper only reports it.
4. **`warpcore-main` retired** after one release.
5. **#233 folded into the engine** rather than merged into the relay.
