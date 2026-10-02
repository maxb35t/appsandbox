#ifndef PROXY_MGR_H
#define PROXY_MGR_H

/* maxb35t fork: the AppSandboxProxy service (tools/asb-proxy) that gives VMs in the
   Proxied network mode filtered web access over Hyper-V socket channel 8.

   The service runs asb-proxy.exe (next to AppSandbox.exe) as NT AUTHORITY\LocalService.
   App Sandbox installs or updates it, writes its policy file, and starts it whenever a
   Proxied VM starts. Files live in %ProgramData%\AppSandbox\proxy\ (policy.txt, proxy.log),
   a folder only Administrators and SYSTEM can change; LocalService can modify it so the
   proxy can write its log. */

#include <windows.h>

#define ASB_PROXY_SERVICE_NAME L"AppSandboxProxy"

/* %ProgramData%\AppSandbox\proxy, created with its ACL if missing. */
BOOL    proxy_mgr_dir(wchar_t *out, size_t cap);

/* Atomically replaces policy.txt (the proxy re-reads it within 2 s). */
HRESULT proxy_mgr_write_policy(const char *utf8_text);

/* Installs or updates the service and starts it if it isn't running. */
HRESULT proxy_mgr_ensure_service(void);

/* TRUE if the service is installed and running. */
BOOL    proxy_mgr_service_running(void);

/* Last `limit` log lines (each a JSON object), newest last, optionally only those whose
   "vm" equals vm_name, as a JSON array in out. Returns the number of lines. */
int     proxy_mgr_read_log(const wchar_t *vm_name, int limit, char *out, size_t cap);

#endif
