"""Windows process access for the hardening scripts, through kernel32 / ntdll / shell32 via ctypes.

Standard library only. Gives `common.py` what it reads from libproc on macOS and `/proc` on Linux:
the process table (pid -> parent), a process's executable path and argv, liveness, and a resource
sample (working set, private bytes, threads, handles, CPU time).
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
from dataclasses import dataclass

_k32 = ctypes.WinDLL("kernel32", use_last_error=True)
_ntdll = ctypes.WinDLL("ntdll")
_shell32 = ctypes.WinDLL("shell32", use_last_error=True)

_TH32CS_SNAPPROCESS = 0x2
_INVALID_HANDLE = ctypes.c_void_p(-1).value
_PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
_PROCESS_VM_READ = 0x0010
_SYNCHRONIZE = 0x00100000
_WAIT_TIMEOUT = 0x102
_ERROR_ACCESS_DENIED = 5
_PROCESS_COMMAND_LINE_INFORMATION = 60
_STATUS_INFO_LENGTH_MISMATCH = 0xC0000004


class _ProcessEntry32W(ctypes.Structure):
    _fields_ = [  # noqa: RUF012 - ctypes layout
        ("dwSize", wintypes.DWORD),
        ("cntUsage", wintypes.DWORD),
        ("th32ProcessID", wintypes.DWORD),
        ("th32DefaultHeapID", ctypes.c_size_t),
        ("th32ModuleID", wintypes.DWORD),
        ("cntThreads", wintypes.DWORD),
        ("th32ParentProcessID", wintypes.DWORD),
        ("pcPriClassBase", wintypes.LONG),
        ("dwFlags", wintypes.DWORD),
        ("szExeFile", wintypes.WCHAR * 260),
    ]


class _MemoryCountersEx(ctypes.Structure):
    _fields_ = [  # noqa: RUF012 - ctypes layout
        ("cb", wintypes.DWORD),
        ("PageFaultCount", wintypes.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
        ("PrivateUsage", ctypes.c_size_t),
    ]


class _UnicodeString(ctypes.Structure):
    _fields_ = [  # noqa: RUF012 - ctypes layout
        ("Length", wintypes.USHORT),
        ("MaximumLength", wintypes.USHORT),
        ("Buffer", ctypes.c_void_p),
    ]


_k32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
_k32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
_k32.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.POINTER(_ProcessEntry32W)]
_k32.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(_ProcessEntry32W)]
_k32.OpenProcess.restype = wintypes.HANDLE
_k32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
_k32.CloseHandle.argtypes = [wintypes.HANDLE]
_k32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
_k32.WaitForSingleObject.restype = wintypes.DWORD
_k32.QueryFullProcessImageNameW.argtypes = [
    wintypes.HANDLE,
    wintypes.DWORD,
    wintypes.LPWSTR,
    ctypes.POINTER(wintypes.DWORD),
]
_k32.K32GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(_MemoryCountersEx), wintypes.DWORD]
_k32.GetProcessTimes.argtypes = [wintypes.HANDLE, *[ctypes.POINTER(wintypes.FILETIME)] * 4]
_k32.GetProcessHandleCount.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
_k32.LocalFree.argtypes = [ctypes.c_void_p]
_ntdll.NtQueryInformationProcess.restype = ctypes.c_ulong
_ntdll.NtQueryInformationProcess.argtypes = [
    wintypes.HANDLE,
    ctypes.c_int,
    ctypes.c_void_p,
    wintypes.ULONG,
    ctypes.POINTER(wintypes.ULONG),
]
_shell32.CommandLineToArgvW.restype = ctypes.POINTER(wintypes.LPWSTR)
_shell32.CommandLineToArgvW.argtypes = [wintypes.LPCWSTR, ctypes.POINTER(ctypes.c_int)]


@dataclass
class Entry:
    ppid: int
    threads: int


def snapshot() -> dict[int, Entry]:
    """pid -> (parent pid, thread count) for every process on the machine."""
    snap = _k32.CreateToolhelp32Snapshot(_TH32CS_SNAPPROCESS, 0)
    if not snap or snap == _INVALID_HANDLE:
        return {}
    table: dict[int, Entry] = {}
    try:
        entry = _ProcessEntry32W()
        entry.dwSize = ctypes.sizeof(entry)
        ok = _k32.Process32FirstW(snap, ctypes.byref(entry))
        while ok:
            table[int(entry.th32ProcessID)] = Entry(int(entry.th32ParentProcessID), int(entry.cntThreads))
            ok = _k32.Process32NextW(snap, ctypes.byref(entry))
    finally:
        _k32.CloseHandle(snap)
    return table


def _open(pid: int, access: int = _PROCESS_QUERY_LIMITED_INFORMATION) -> int | None:
    handle = _k32.OpenProcess(access, False, pid)
    return handle or None


def exe_path(pid: int) -> str | None:
    handle = _open(pid)
    if handle is None:
        return None
    try:
        size = wintypes.DWORD(32768)
        buf = ctypes.create_unicode_buffer(size.value)
        if not _k32.QueryFullProcessImageNameW(handle, 0, buf, ctypes.byref(size)):
            return None
        return buf.value
    finally:
        _k32.CloseHandle(handle)


def argv(pid: int) -> list[str] | None:
    """The command line of `pid`, split the way the C runtime splits it."""
    handle = _open(pid)
    if handle is None:
        return None
    try:
        need = wintypes.ULONG(0)
        status = _ntdll.NtQueryInformationProcess(
            handle, _PROCESS_COMMAND_LINE_INFORMATION, None, 0, ctypes.byref(need)
        )
        if status not in (0, _STATUS_INFO_LENGTH_MISMATCH) or need.value == 0:
            return None
        buf = ctypes.create_string_buffer(need.value)
        status = _ntdll.NtQueryInformationProcess(
            handle, _PROCESS_COMMAND_LINE_INFORMATION, buf, need, ctypes.byref(need)
        )
        if status != 0:
            return None
        ustr = _UnicodeString.from_buffer(buf)
        if not ustr.Buffer or not ustr.Length:
            return []
        line = ctypes.wstring_at(ustr.Buffer, ustr.Length // 2)
    finally:
        _k32.CloseHandle(handle)
    count = ctypes.c_int(0)
    parts = _shell32.CommandLineToArgvW(line, ctypes.byref(count))
    if not parts:
        return [line]
    try:
        return [parts[i] for i in range(count.value)]
    finally:
        _k32.LocalFree(parts)


def alive(pid: int) -> bool:
    handle = _open(pid, _SYNCHRONIZE | _PROCESS_QUERY_LIMITED_INFORMATION)
    if handle is None:
        return ctypes.get_last_error() == _ERROR_ACCESS_DENIED
    try:
        return _k32.WaitForSingleObject(handle, 0) == _WAIT_TIMEOUT
    finally:
        _k32.CloseHandle(handle)


@dataclass
class Sample:
    working_set: int
    private: int
    threads: int
    handles: int
    cpu_s: float


def sample(pid: int, threads: int) -> Sample | None:
    """Resource sample of `pid`; `threads` comes from the caller's `snapshot()`."""
    handle = _open(pid, _PROCESS_QUERY_LIMITED_INFORMATION | _PROCESS_VM_READ)
    if handle is None:
        handle = _open(pid)
    if handle is None:
        return None
    try:
        mem = _MemoryCountersEx()
        mem.cb = ctypes.sizeof(mem)
        if not _k32.K32GetProcessMemoryInfo(handle, ctypes.byref(mem), mem.cb):
            return None
        times = [wintypes.FILETIME() for _ in range(4)]
        if not _k32.GetProcessTimes(handle, *[ctypes.byref(t) for t in times]):
            return None
        count = wintypes.DWORD(0)
        _k32.GetProcessHandleCount(handle, ctypes.byref(count))
    finally:
        _k32.CloseHandle(handle)

    def ticks(ft: wintypes.FILETIME) -> int:
        return (ft.dwHighDateTime << 32) | ft.dwLowDateTime

    return Sample(
        working_set=int(mem.WorkingSetSize),
        private=int(mem.PrivateUsage),
        threads=threads,
        handles=int(count.value),
        cpu_s=(ticks(times[2]) + ticks(times[3])) / 1e7,
    )
