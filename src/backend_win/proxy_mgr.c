#include "proxy_mgr.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sddl.h>
#include <aclapi.h>

#pragma comment(lib, "advapi32.lib")

/* Administrators and SYSTEM: full; LocalService (the proxy): modify (write its log).
   Protected, so nothing is inherited from ProgramData (where Users can create files). */
#define PROXY_DIR_SDDL L"D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;0x1301bf;;;LS)"

BOOL proxy_mgr_dir(wchar_t *out, size_t cap)
{
    wchar_t base[MAX_PATH];
    PSECURITY_DESCRIPTOR sd = NULL;
    PACL dacl = NULL;
    BOOL present = FALSE, defaulted = FALSE;
    DWORD n = GetEnvironmentVariableW(L"ProgramData", base, MAX_PATH);
    if (!n || n >= MAX_PATH) wcscpy_s(base, MAX_PATH, L"C:\\ProgramData");
    if (_snwprintf_s(out, cap, _TRUNCATE, L"%s\\AppSandbox\\proxy", base) < 0) return FALSE;
    CreateDirectoryW(out, NULL);
    if (GetFileAttributesW(out) == INVALID_FILE_ATTRIBUTES) return FALSE;
    /* (Re)apply the ACL every time: cheap, and repairs a folder created another way. */
    if (ConvertStringSecurityDescriptorToSecurityDescriptorW(PROXY_DIR_SDDL, SDDL_REVISION_1, &sd, NULL)) {
        if (GetSecurityDescriptorDacl(sd, &present, &dacl, &defaulted) && present)
            SetNamedSecurityInfoW(out, SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, NULL, NULL, dacl, NULL);
        LocalFree(sd);
    }
    return TRUE;
}

HRESULT proxy_mgr_write_policy(const char *utf8_text)
{
    wchar_t dir[MAX_PATH], tmp[MAX_PATH], dst[MAX_PATH];
    HANDLE h;
    DWORD written = 0, len;
    if (!utf8_text || !proxy_mgr_dir(dir, MAX_PATH)) return E_FAIL;
    swprintf_s(tmp, MAX_PATH, L"%s\\policy.tmp", dir);
    swprintf_s(dst, MAX_PATH, L"%s\\policy.txt", dir);
    h = CreateFileW(tmp, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) return HRESULT_FROM_WIN32(GetLastError());
    len = (DWORD)strlen(utf8_text);
    if (!WriteFile(h, utf8_text, len, &written, NULL) || written != len) {
        DWORD e = GetLastError();
        CloseHandle(h);
        DeleteFileW(tmp);
        return HRESULT_FROM_WIN32(e ? e : ERROR_WRITE_FAULT);
    }
    FlushFileBuffers(h);
    CloseHandle(h);
    if (!MoveFileExW(tmp, dst, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH))
        return HRESULT_FROM_WIN32(GetLastError());
    return S_OK;
}

static BOOL proxy_bin_path(wchar_t *bin, size_t cap, wchar_t *exe, size_t exe_cap)
{
    wchar_t dir[MAX_PATH], pdir[MAX_PATH], *slash;
    if (!GetModuleFileNameW(NULL, dir, MAX_PATH)) return FALSE;
    slash = wcsrchr(dir, L'\\');
    if (!slash) return FALSE;
    *slash = L'\0';
    if (_snwprintf_s(exe, exe_cap, _TRUNCATE, L"%s\\asb-proxy.exe", dir) < 0) return FALSE;
    if (!proxy_mgr_dir(pdir, MAX_PATH)) return FALSE;
    return _snwprintf_s(bin, cap, _TRUNCATE,
        L"\"%s\" service --policy \"%s\\policy.txt\" --log-dir \"%s\"", exe, pdir, pdir) >= 0;
}

static BOOL wait_running(SC_HANDLE svc, DWORD timeout_ms)
{
    SERVICE_STATUS st;
    DWORD waited = 0;
    while (QueryServiceStatus(svc, &st)) {
        if (st.dwCurrentState == SERVICE_RUNNING) return TRUE;
        if (st.dwCurrentState == SERVICE_STOPPED && waited > 500) return FALSE;
        if (waited >= timeout_ms) return FALSE;
        Sleep(100); waited += 100;
    }
    return FALSE;
}

HRESULT proxy_mgr_ensure_service(void)
{
    static const wchar_t *account = L"NT AUTHORITY\\LocalService";
    wchar_t bin[1024], exe[MAX_PATH];
    SC_HANDLE scm, svc;
    SERVICE_STATUS st;
    BOOL changed = FALSE;
    HRESULT hr = S_OK;

    if (!proxy_bin_path(bin, ARRAYSIZE(bin), exe, MAX_PATH)) return E_FAIL;
    if (GetFileAttributesW(exe) == INVALID_FILE_ATTRIBUTES)
        return HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND);

    scm = OpenSCManagerW(NULL, NULL, SC_MANAGER_ALL_ACCESS);
    if (!scm) return HRESULT_FROM_WIN32(GetLastError());
    svc = OpenServiceW(scm, ASB_PROXY_SERVICE_NAME, SERVICE_ALL_ACCESS);
    if (!svc) {
        svc = CreateServiceW(scm, ASB_PROXY_SERVICE_NAME, L"App Sandbox proxy", SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS, SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL,
            bin, NULL, NULL, NULL, account, L"");
        if (!svc) { hr = HRESULT_FROM_WIN32(GetLastError()); CloseServiceHandle(scm); return hr; }
        changed = TRUE;
    } else {
        /* Bring an existing service (e.g. one installed by hand) in line. */
        BYTE cfgbuf[8192];
        QUERY_SERVICE_CONFIGW *cfg = (QUERY_SERVICE_CONFIGW *)cfgbuf;
        DWORD need = 0;
        if (QueryServiceConfigW(svc, cfg, sizeof(cfgbuf), &need) &&
            (!cfg->lpBinaryPathName || _wcsicmp(cfg->lpBinaryPathName, bin) != 0 ||
             !cfg->lpServiceStartName || _wcsicmp(cfg->lpServiceStartName, account) != 0)) {
            if (!ChangeServiceConfigW(svc, SERVICE_NO_CHANGE, SERVICE_NO_CHANGE, SERVICE_NO_CHANGE,
                                      bin, NULL, NULL, NULL, account, L"", NULL)) {
                hr = HRESULT_FROM_WIN32(GetLastError());
                CloseServiceHandle(svc); CloseServiceHandle(scm);
                return hr;
            }
            changed = TRUE;
        }
    }
    if (changed) {
        SC_ACTION actions[3] = { { SC_ACTION_RESTART, 2000 }, { SC_ACTION_RESTART, 2000 }, { SC_ACTION_RESTART, 10000 } };
        SERVICE_FAILURE_ACTIONSW fa;
        SERVICE_DESCRIPTIONW desc;
        ZeroMemory(&fa, sizeof(fa));
        fa.dwResetPeriod = 86400;
        fa.cActions = 3;
        fa.lpsaActions = actions;
        ChangeServiceConfig2W(svc, SERVICE_CONFIG_FAILURE_ACTIONS, &fa);
        desc.lpDescription = L"Filtering HTTP/HTTPS proxy for App Sandbox VMs in the Proxied network mode (maxb35t fork).";
        ChangeServiceConfig2W(svc, SERVICE_CONFIG_DESCRIPTION, &desc);
        /* A running copy still has the old configuration: restart it. */
        if (QueryServiceStatus(svc, &st) && st.dwCurrentState == SERVICE_RUNNING) {
            ControlService(svc, SERVICE_CONTROL_STOP, &st);
            { int i; for (i = 0; i < 50 && QueryServiceStatus(svc, &st) && st.dwCurrentState != SERVICE_STOPPED; i++) Sleep(100); }
        }
    }
    if (QueryServiceStatus(svc, &st) && st.dwCurrentState != SERVICE_RUNNING) {
        if (!StartServiceW(svc, 0, NULL) && GetLastError() != ERROR_SERVICE_ALREADY_RUNNING)
            hr = HRESULT_FROM_WIN32(GetLastError());
        else if (!wait_running(svc, 5000))
            hr = HRESULT_FROM_WIN32(ERROR_SERVICE_REQUEST_TIMEOUT);
    }
    CloseServiceHandle(svc);
    CloseServiceHandle(scm);
    return hr;
}

BOOL proxy_mgr_service_running(void)
{
    SC_HANDLE scm = OpenSCManagerW(NULL, NULL, SC_MANAGER_CONNECT), svc;
    SERVICE_STATUS st;
    BOOL running = FALSE;
    if (!scm) return FALSE;
    svc = OpenServiceW(scm, ASB_PROXY_SERVICE_NAME, SERVICE_QUERY_STATUS);
    if (svc) {
        running = QueryServiceStatus(svc, &st) && st.dwCurrentState == SERVICE_RUNNING;
        CloseServiceHandle(svc);
    }
    CloseServiceHandle(scm);
    return running;
}

int proxy_mgr_read_log(const wchar_t *vm_name, int limit, char *out, size_t cap)
{
    wchar_t dir[MAX_PATH], path[MAX_PATH];
    char needle[600] = { 0 }, *buf, *p, *end;
    const char **lines;
    HANDLE h;
    LARGE_INTEGER size;
    DWORD want, got = 0;
    int count = 0, start, i;
    size_t pos = 0;

    if (!out || cap < 3) return 0;
    strcpy_s(out, cap, "[]");
    if (limit <= 0) limit = 100;
    if (limit > 2000) limit = 2000;
    if (!proxy_mgr_dir(dir, MAX_PATH)) return 0;
    swprintf_s(path, MAX_PATH, L"%s\\proxy.log", dir);
    if (vm_name && vm_name[0]) {
        char name[512];
        WideCharToMultiByte(CP_UTF8, 0, vm_name, -1, name, sizeof(name), NULL, NULL);
        sprintf_s(needle, sizeof(needle), "\"vm\":\"%s\"", name);
    }
    h = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) return 0;
    if (!GetFileSizeEx(h, &size)) { CloseHandle(h); return 0; }
    want = (DWORD)(size.QuadPart > 1024 * 1024 ? 1024 * 1024 : size.QuadPart);
    buf = (char *)malloc((size_t)want + 1);
    lines = (const char **)malloc(sizeof(char *) * 20000);
    if (!buf || !lines) { free(buf); free((void *)lines); CloseHandle(h); return 0; }
    { LARGE_INTEGER off; off.QuadPart = size.QuadPart - want; SetFilePointerEx(h, off, NULL, FILE_BEGIN); }
    ReadFile(h, buf, want, &got, NULL);
    CloseHandle(h);
    buf[got] = '\0';
    p = buf;
    if (size.QuadPart > want) { char *nl = strchr(p, '\n'); p = nl ? nl + 1 : p + got; }   /* skip partial line */
    while (*p && count < 20000) {
        end = strchr(p, '\n');
        if (end) *end = '\0';
        if (p[0] == '{' && (!needle[0] || strstr(p, needle))) lines[count++] = p;
        if (!end) break;
        p = end + 1;
    }
    start = count > limit ? count - limit : 0;
    pos = 0;
    out[pos++] = '[';
    for (i = start; i < count; i++) {
        size_t l = strlen(lines[i]);
        if (pos + l + 3 >= cap) break;
        if (i > start) out[pos++] = ',';
        memcpy(out + pos, lines[i], l);
        pos += l;
    }
    out[pos++] = ']';
    out[pos] = '\0';
    free(buf);
    free((void *)lines);
    return count - start;
}
