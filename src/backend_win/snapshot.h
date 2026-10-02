#ifndef SNAPSHOT_H
#define SNAPSHOT_H

#include <windows.h>
#include "hcs_vm.h"

/* DLL export/import */
#ifndef ASB_API
#ifdef ASB_BUILDING_DLL
#define ASB_API __declspec(dllexport)
#else
#define ASB_API __declspec(dllimport)
#endif
#endif

#define MAX_SNAPSHOTS 64
#define MAX_BRANCHES  8

/*
 * Snapshot tree — frozen disks layered on the base, each with working branches.
 *
 * maxb35t fork: snapshots can chain. Taking a snapshot while on a branch freezes that
 * branch's disk as the new snapshot (its parent is the snapshot or base the branch was on)
 * and continues on a fresh branch layered on it. Upstream always layered a new, empty
 * snapshot on the base, so a branch's changes were silently left out.
 *
 * Filesystem layout:
 *   MyVM/
 *     disk.vhdx                              <- base disk (frozen once snapshots exist)
 *     snapshots/
 *       tree.dat                             <- persisted tree metadata (GUIDs + friendly names)
 *       snapshot_{GUID}.vhdx                <- frozen fork off base
 *       branch_{GUID}.vhdx                 <- working branch (off base or snapshot)
 *
 * Snapshots and base are frozen — never booted directly.
 * Booting creates/resumes a branch (differencing VHDX of the snapshot or base).
 * Each snapshot/base can have multiple independent branches.
 */

typedef struct {
    wchar_t  guid[64];
    wchar_t  friendly_name[128];
    wchar_t  vhdx_path[MAX_PATH];
    BOOL     valid;
} BranchEntry;

typedef struct {
    wchar_t      guid[64];
    wchar_t      name[128];            /* editable friendly name */
    wchar_t      snap_vhdx[MAX_PATH];  /* frozen snapshot disk (diff of its parent) */
    wchar_t      parent_guid[64];      /* fork: parent snapshot's guid; empty = the base */
    FILETIME     created;
    BOOL         valid;
    BranchEntry  branches[MAX_BRANCHES];
    int          branch_count;
} SnapNode;

typedef struct {
    SnapNode     nodes[MAX_SNAPSHOTS];
    int          count;
    wchar_t      base_dir[MAX_PATH];    /* snapshots directory */
    wchar_t      base_vhdx[MAX_PATH];   /* original base disk (frozen) */
    BranchEntry  base_branches[MAX_BRANCHES];
    int          base_branch_count;
} SnapshotTree;

/* Initialize snapshot tree.  Loads tree.dat from base_dir if it exists. */
void snapshot_init(SnapshotTree *tree, const wchar_t *base_dir);

/* Persist snapshot tree metadata to tree.dat. */
void snapshot_save(SnapshotTree *tree);

/* Take a new snapshot of the VM's current disk. VM must be stopped.
   On a branch: the branch's disk becomes the frozen snapshot (renamed to
   snapshot_{GUID}.vhdx), parented on the branch's snapshot (or the base), and a new
   branch with the old branch's name continues on top of it.
   On the base or a frozen snapshot disk: a new, empty snapshot is layered on it, with a
   "Default Branch" (upstream behaviour for the base).
   Sets instance->vhdx_path to the new branch. base_vhdx is captured from
   instance->vhdx_path on the first call. */
HRESULT snapshot_take(SnapshotTree *tree, VmInstance *instance, const wchar_t *name);

/* Create a new branch off a snapshot or base.
   index >= 0: branch off snapshot[index].  index == -2: branch off base.
   Sets instance->vhdx_path to the new branch. */
HRESULT snapshot_new_branch(SnapshotTree *tree, VmInstance *instance, int index);

/* Select an existing branch for booting.
   index >= 0: snapshot.  index == -2: base.  branch_idx: which branch.
   Sets instance->vhdx_path accordingly. */
HRESULT snapshot_select_branch(SnapshotTree *tree, VmInstance *instance, int index, int branch_idx);

/* Fork a frozen disk before booting. S_FALSE if the selected disk is unchanged. */
HRESULT snapshot_ensure_writable(SnapshotTree *tree, VmInstance *instance);

/* Delete a snapshot and all its branches.
   fork: refused with HRESULT_FROM_WIN32(ERROR_DIR_NOT_EMPTY) while other snapshots are
   layered on it (delete those first). */
HRESULT snapshot_delete(SnapshotTree *tree, VmInstance *instance, int index);

/* Delete a single branch.
   index >= 0: snapshot branch.  index == -2: base branch. */
HRESULT snapshot_delete_branch(SnapshotTree *tree, VmInstance *instance, int index, int branch_idx);

/* Find which snapshot and branch match vhdx_path.
   Sets *snap_idx (-2=base, >=0=snapshot, -1=unknown) and *branch_idx (-1 if none). */
ASB_API void snapshot_find_current(SnapshotTree *tree, const wchar_t *vhdx_path, int *snap_idx, int *branch_idx);

/* fork: parent of snapshot[index]: -2 = the base, >= 0 = another snapshot's index,
   -1 = invalid index or parent not found. */
ASB_API int snapshot_parent_index(SnapshotTree *tree, int index);

/* fork: TRUE if any snapshot is layered on snapshot[index]. */
ASB_API BOOL snapshot_has_children(SnapshotTree *tree, int index);

/* Get the last-write time of a branch file.  Returns FALSE if not found. */
ASB_API BOOL snapshot_get_branch_time(SnapshotTree *tree, int snap_idx, int branch_idx, FILETIME *ft);

/* Rename a snapshot or branch friendly name.
   snap_idx >= 0, branch_idx == -1: rename snapshot.
   snap_idx >= 0 or -2, branch_idx >= 0: rename branch. */
HRESULT snapshot_rename(SnapshotTree *tree, int snap_idx, int branch_idx, const wchar_t *new_name);

#endif /* SNAPSHOT_H */
