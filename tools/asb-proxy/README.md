# asb-proxy

Filtering HTTP/HTTPS proxy for App Sandbox VMs (maxb35t fork). It gives a VM with
**no network adapter** web access that is checked per VM.

```
app in VM ──HTTP(S)_PROXY──▶ asb-proxy guest (127.0.0.1:3128 in the VM)
          ──Hyper-V socket, channel port 8──▶ asb-proxy host (Windows service, LocalService)
          ──▶ the internet, if the VM's policy allows it
```

- **Guest mode** only relays bytes. Every check runs in **host mode**, outside the VM.
- **Host mode:**
  - identifies the VM from the connection's VM id;
  - reads only the first request head (`CONNECT host:port`, or an absolute `http://` URL);
  - checks port, deny list and allow list;
  - resolves the name, then **drops every private, loopback, link-local, multicast or reserved address** before connecting, so a DNS answer can't redirect it to the LAN or the host;
  - tunnels a `CONNECT`, or forwards one plain-HTTP request with `Connection: close`.
- **Every connection** is logged as JSON lines in `proxy.log`, which is rotated at 10 MB.
  - A connection that gets through writes an `"phase":"open"` line as soon as it's connected, then a `"phase":"close"` line with the bytes and duration when it ends. Both lines carry the same `id`.
  - A refused or failed connection writes only a `close` line.
  - Connections that never send a request aren't logged. Browsers open these spares in advance.
- **No dependencies.** It reads traffic from untrusted VMs, so it's kept small enough to review in full, and platform calls are hand-declared FFI.

## Policy file

App Sandbox writes this file. VMs with no `[vm ...]` section are refused unless `allow_unknown=1`.

```
[default]
allow_unknown=0
block_private=1
ports=80,443
allow=                 # empty = any host; entries match the host and its subdomains
deny=
log=1
max_conns=64
[vm 01234567-89ab-cdef-0123-456789abcdef]
name=AgentTest-1
allow=github.com, crates.io
```

The proxy re-reads the file within 2 seconds of any change.

## Commands

```
asb-proxy host    --policy FILE [--log-dir DIR] [--port 8]
asb-proxy service --policy FILE [--log-dir DIR] [--port 8]   (as the AppSandboxProxy service)
asb-proxy guest   [127.0.0.1:3128] [--port 8]                (Windows: Hyper-V socket, Linux: vsock)
asb-proxy check-policy FILE
```

The host listens for both Windows guests (`a5b0cafe-0008-4000-8000-000000000001`) and Linux
guests (`00000008-facb-11e6-bd58-64006a7986d3`). Both need the VM's `relayChannel` setting on.

## Tests

`cargo test` (runs on any OS) covers:
- request parsing and rewriting;
- the address filter;
- policy rules;
- end-to-end proxying over local TCP.
