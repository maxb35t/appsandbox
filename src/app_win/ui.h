#ifndef UI_H
#define UI_H

#include <windows.h>

/* DLL export/import — matches asb_core.h */
#ifndef ASB_API
#ifdef ASB_BUILDING_DLL
#define ASB_API __declspec(dllexport)
#else
#define ASB_API __declspec(dllimport)
#endif
#endif

/* Create and show the main application window.
   Returns the window handle, or NULL on failure. */
HWND ui_create_main_window(HINSTANCE hInstance, int nCmdShow);

/* ---- fork: the GUI and the headless daemon at the same time ----
   The daemon runs the GUI's own action handler for an attached GUI (ui_served_*),
   and a GUI started while the daemon runs attaches to it (ui_attach_probe). */
void ui_served_init(void);                    /* daemon: call on the request thread */
void ui_served_action(const wchar_t *json);   /* daemon: one GUI action message */
void ui_served_refresh(void);                 /* daemon: push the VM list */
void ui_served_log(const wchar_t *msg);       /* daemon: a line for the GUI's log panel */
void ui_served_alert(const wchar_t *msg);     /* daemon: an alert in the GUI */
BOOL ui_attach_probe(void);                   /* GUI: TRUE (attach mode) if the daemon answers */

/* Write a line to appsandbox.log and forward it to the registered
   AsbLogCallback (if any). Thread-safe: can be called from any thread.
   Implemented in asb_core.c (DLL). */
ASB_API void ui_log(const wchar_t *fmt, ...);

/* Get the HINSTANCE used to create the main window.
   Implemented in asb_core.c (DLL). */
ASB_API HINSTANCE ui_get_instance(void);

/* Save/load per-VM state JSON file (beside disk.vhdx).
   Implemented in asb_core.c (DLL). */
ASB_API void vm_save_state_json(const wchar_t *vhdx_path, BOOL install_complete);
ASB_API BOOL vm_load_state_json(const wchar_t *vhdx_path);

#endif /* UI_H */
