#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif

#include <windows.h>
#include <iostream>
#include <string>
#include <vector>
#include <cstdint>
#include <climits>
#include <algorithm>

class SafeHandle {
    HANDLE handle_;
public:
    explicit SafeHandle(HANDLE h = NULL) : handle_(h) {}
    ~SafeHandle() {
        close();
    }
    SafeHandle(const SafeHandle&) = delete;
    SafeHandle& operator=(const SafeHandle&) = delete;
    SafeHandle(SafeHandle&& other) noexcept : handle_(other.handle_) {
        other.handle_ = NULL;
    }
    SafeHandle& operator=(SafeHandle&& other) noexcept {
        if (this != &other) {
            close();
            handle_ = other.handle_;
            other.handle_ = NULL;
        }
        return *this;
    }
    HANDLE get() const { return handle_; }
    HANDLE release() {
        HANDLE h = handle_;
        handle_ = NULL;
        return h;
    }
    void reset(HANDLE h = NULL) {
        close();
        handle_ = h;
    }
    void close() {
        if (handle_ && handle_ != INVALID_HANDLE_VALUE) {
            CloseHandle(handle_);
            handle_ = NULL;
        }
    }
    bool is_valid() const { return handle_ != NULL && handle_ != INVALID_HANDLE_VALUE; }
    operator bool() const { return is_valid(); }
};

static void terminate_and_wait(HANDLE hProcess, DWORD timeout_ms = 30000) {
    if (hProcess && hProcess != INVALID_HANDLE_VALUE) {
        DWORD exitCode = 0;
        if (GetExitCodeProcess(hProcess, &exitCode) && exitCode != STILL_ACTIVE) {
            return;
        }
        TerminateProcess(hProcess, 1);
        WaitForSingleObject(hProcess, timeout_ms);
    }
}

static bool parse_uint32(const std::string& str, uint32_t& out) {
    if (str.empty()) return false;
    for (char c : str) {
        if (c < '0' || c > '9') return false;
    }
    try {
        size_t idx = 0;
        unsigned long val = std::stoul(str, &idx, 10);
        if (idx != str.length() || val == 0 || val > 0xFFFFFFFFUL) {
            return false;
        }
        out = static_cast<uint32_t>(val);
        return true;
    } catch (...) {
        return false;
    }
}

static bool parse_uint64(const std::string& str, uint64_t& out) {
    if (str.empty()) return false;
    for (char c : str) {
        if (c < '0' || c > '9') return false;
    }
    try {
        size_t idx = 0;
        unsigned long long val = std::stoull(str, &idx, 10);
        if (idx != str.length() || val == 0) {
            return false;
        }
        out = static_cast<uint64_t>(val);
        return true;
    } catch (...) {
        return false;
    }
}

static bool decode_base64(const std::string& input, std::vector<uint8_t>& out) {
    out.clear();
    if (input.empty()) {
        return true;
    }
    if (input.length() % 4 != 0) {
        return false;
    }

    static const int dec_table[256] = {
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,62, -1,-1,-1,63,
        52,53,54,55, 56,57,58,59, 60,61,-1,-1, -1, -2,-1,-1,
        -1, 0, 1, 2,  3, 4, 5, 6,  7, 8, 9,10, 11,12,13,14,
        15,16,17,18, 19,20,21,22, 23,24,25,-1, -1,-1,-1,-1,
        -1,26,27,28, 29,30,31,32, 33,34,35,36, 37,38,39,40,
        41,42,43,44, 45,46,47,48, 49,50,51,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1,
        -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1, -1,-1,-1,-1
    };

    size_t len = input.length();
    for (size_t i = 0; i < len; i += 4) {
        int c0 = dec_table[static_cast<unsigned char>(input[i])];
        int c1 = dec_table[static_cast<unsigned char>(input[i + 1])];
        int c2 = dec_table[static_cast<unsigned char>(input[i + 2])];
        int c3 = dec_table[static_cast<unsigned char>(input[i + 3])];

        if (c0 < 0 || c1 < 0) return false;

        if (c2 == -2) {
            if (c3 != -2) return false;
            if (i + 4 != len) return false;
            if ((c1 & 0x0F) != 0) return false;
        } else if (c2 < 0) {
            return false;
        } else if (c3 == -2) {
            if (i + 4 != len) return false;
            if ((c2 & 0x03) != 0) return false;
        } else if (c3 < 0) {
            return false;
        }

        uint32_t triple = (static_cast<uint32_t>(c0) << 18) |
                          (static_cast<uint32_t>(c1) << 12) |
                          (static_cast<uint32_t>(c2 < 0 ? 0 : c2) << 6) |
                          (static_cast<uint32_t>(c3 < 0 ? 0 : c3));

        out.push_back(static_cast<uint8_t>((triple >> 16) & 0xFF));
        if (c2 != -2) {
            out.push_back(static_cast<uint8_t>((triple >> 8) & 0xFF));
        }
        if (c3 != -2) {
            out.push_back(static_cast<uint8_t>(triple & 0xFF));
        }
    }
    return true;
}

static bool utf8_to_wstring(const std::vector<uint8_t>& bytes, std::wstring& out) {
    out.clear();
    if (bytes.empty()) {
        return true;
    }
    if (bytes.size() > static_cast<size_t>(INT_MAX)) {
        return false;
    }
    int wlen = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, reinterpret_cast<const char*>(bytes.data()), static_cast<int>(bytes.size()), NULL, 0);
    if (wlen <= 0) {
        return false;
    }
    out.resize(wlen);
    int res = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, reinterpret_cast<const char*>(bytes.data()), static_cast<int>(bytes.size()), &out[0], wlen);
    return res > 0;
}

static bool decode_base64_utf8(const std::string& input, std::wstring& out) {
    std::vector<uint8_t> bytes;
    if (!decode_base64(input, bytes)) {
        return false;
    }
    return utf8_to_wstring(bytes, out);
}

static std::wstring normalize_path(const std::wstring& path) {
    std::wstring result = path;
    for (auto& ch : result) {
        if (ch == L'/') ch = L'\\';
    }
    if (result.rfind(L"\\\\?\\UNC\\", 0) == 0) {
        result = L"\\\\" + result.substr(8);
    } else if (result.rfind(L"\\\\?\\", 0) == 0) {
        result = result.substr(4);
    }

    wchar_t buf[32768];
    DWORD len = GetFullPathNameW(result.c_str(), 32768, buf, NULL);
    if (len > 0 && len < 32768) {
        result = std::wstring(buf, len);
    }

    while (result.length() > 3 && result.back() == L'\\') {
        result.pop_back();
    }
    return result;
}

static bool paths_equal(const std::wstring& path1, const std::wstring& path2) {
    std::wstring norm1 = normalize_path(path1);
    std::wstring norm2 = normalize_path(path2);
    return _wcsicmp(norm1.c_str(), norm2.c_str()) == 0;
}

static int cmd_launch(
    const std::string& studio_b64,
    const std::string& place_b64,
    const std::string& managed_str,
    const std::string& loader_b64,
    const std::string& build_b64,
    const std::string& dotnet_root_b64
) {
    if (managed_str != "0" && managed_str != "1") {
        std::cerr << "Invalid managed flag (must be 0 or 1)\n";
        return 1;
    }
    bool is_managed = (managed_str == "1");

    std::wstring studio_path;
    if (!decode_base64_utf8(studio_b64, studio_path) || studio_path.empty()) {
        std::cerr << "Invalid base64/UTF-8 studio path\n";
        return 1;
    }

    std::wstring place_path;
    if (!decode_base64_utf8(place_b64, place_path)) {
        std::cerr << "Invalid base64/UTF-8 place path\n";
        return 1;
    }

    if (studio_path.find(L'\0') != std::wstring::npos || place_path.find(L'\0') != std::wstring::npos) {
        std::cerr << "Studio and place paths must not contain NUL characters\n";
        return 1;
    }

    std::wstring loader_path;
    if (!decode_base64_utf8(loader_b64, loader_path) || loader_path.empty() ||
        loader_path.find(L'\0') != std::wstring::npos) {
        std::cerr << "Invalid base64/UTF-8 RML loader path\n";
        return 1;
    }

    std::wstring build_version;
    if (!decode_base64_utf8(build_b64, build_version) || build_version.empty() ||
        build_version.find(L'\0') != std::wstring::npos) {
        std::cerr << "Invalid base64/UTF-8 RML build version\n";
        return 1;
    }

    std::wstring dotnet_root;
    if (!decode_base64_utf8(dotnet_root_b64, dotnet_root) || dotnet_root.empty() ||
        dotnet_root.find(L'\0') != std::wstring::npos) {
        std::cerr << "Invalid base64/UTF-8 .NET root\n";
        return 1;
    }

    struct EnvironmentValue {
        const wchar_t* name;
        const std::wstring* value;
    };
    const EnvironmentValue environment[] = {
        { L"CARBON_RML_LOADER", &loader_path },
        { L"CARBON_RML_BUILD_VERSION", &build_version },
        { L"DOTNET_ROOT", &dotnet_root },
    };
    for (const auto& entry : environment) {
        if (!SetEnvironmentVariableW(entry.name, entry.value->c_str())) {
            std::cerr << "SetEnvironmentVariableW failed for a required Studio environment value: "
                      << GetLastError() << "\n";
            return 1;
        }
    }
    if (!SetEnvironmentVariableW(L"CARBON_RML_LOADED_BUILD_VERSION", NULL)) {
        std::cerr << "SetEnvironmentVariableW failed while clearing the loaded RML build marker: "
                  << GetLastError() << "\n";
        return 1;
    }

    SafeHandle hJob(CreateJobObjectW(NULL, NULL));
    if (!hJob) {
        std::cerr << "CreateJobObjectW failed: " << GetLastError() << "\n";
        return 1;
    }

    JOBOBJECT_EXTENDED_LIMIT_INFORMATION limitInfo = { 0 };
    limitInfo.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if (!SetInformationJobObject(hJob.get(), JobObjectExtendedLimitInformation, &limitInfo, sizeof(limitInfo))) {
        std::cerr << "SetInformationJobObject failed: " << GetLastError() << "\n";
        return 1;
    }

    std::wstring cmdline;
    if (is_managed) {
        if (!place_path.empty()) {
            cmdline = L"\"" + studio_path + L"\" --task EditFile --localPlaceFile \"" + place_path + L"\"";
        } else {
            cmdline = L"\"" + studio_path + L"\" --task EditFile";
        }
    } else {
        if (!place_path.empty()) {
            cmdline = L"\"" + studio_path + L"\" \"" + place_path + L"\"";
        } else {
            cmdline = L"\"" + studio_path + L"\"";
        }
    }

    std::wstring workdir;
    size_t last_slash = studio_path.find_last_of(L"\\/");
    if (last_slash != std::wstring::npos) {
        workdir = studio_path.substr(0, last_slash);
    }

    STARTUPINFOW si = { sizeof(si) };
    PROCESS_INFORMATION pi = { 0 };

    std::vector<wchar_t> cmdline_buf(cmdline.begin(), cmdline.end());
    cmdline_buf.push_back(L'\0');

    BOOL created = CreateProcessW(
        studio_path.c_str(),
        cmdline_buf.data(),
        NULL,
        NULL,
        FALSE,
        CREATE_SUSPENDED | CREATE_BREAKAWAY_FROM_JOB,
        NULL,
        workdir.empty() ? NULL : workdir.c_str(),
        &si,
        &pi
    );

    if (!created && GetLastError() == ERROR_ACCESS_DENIED) {
        std::vector<wchar_t> fallback_cmdline_buf(cmdline.begin(), cmdline.end());
        fallback_cmdline_buf.push_back(L'\0');
        created = CreateProcessW(
            studio_path.c_str(),
            fallback_cmdline_buf.data(),
            NULL,
            NULL,
            FALSE,
            CREATE_SUSPENDED,
            NULL,
            workdir.empty() ? NULL : workdir.c_str(),
            &si,
            &pi
        );
    }

    if (!created) {
        std::cerr << "CreateProcessW failed: " << GetLastError() << "\n";
        return 1;
    }

    SafeHandle hProcess(pi.hProcess);
    SafeHandle hThread(pi.hThread);

    if (!AssignProcessToJobObject(hJob.get(), hProcess.get())) {
        std::cerr << "AssignProcessToJobObject failed: " << GetLastError() << "\n";
        terminate_and_wait(hProcess.get(), 30000);
        return 1;
    }

    FILETIME ftCreate, ftExit, ftKernel, ftUser;
    if (!GetProcessTimes(hProcess.get(), &ftCreate, &ftExit, &ftKernel, &ftUser)) {
        std::cerr << "GetProcessTimes failed: " << GetLastError() << "\n";
        terminate_and_wait(hProcess.get(), 30000);
        return 1;
    }

    ULARGE_INTEGER uli;
    uli.LowPart = ftCreate.dwLowDateTime;
    uli.HighPart = ftCreate.dwHighDateTime;
    uint64_t startedAtFileTime = uli.QuadPart;

    std::cout << pi.dwProcessId << "\n" << startedAtFileTime << std::endl;

    bool completed = false;

    struct LaunchGuard {
        SafeHandle& job;
        SafeHandle& process;
        SafeHandle& thread;
        bool& completed;
        ~LaunchGuard() {
            if (!completed && process.is_valid()) {
                terminate_and_wait(process.get(), 30000);
            } else if (completed && job.is_valid()) {
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION info = { 0 };
                SetInformationJobObject(job.get(), JobObjectExtendedLimitInformation, &info, sizeof(info));
            }
        }
    } guard{ hJob, hProcess, hThread, completed };

    std::string line;
    while (std::getline(std::cin, line)) {
        if (!line.empty() && line.back() == '\r') line.pop_back();

        if (line == "CARBON_STUDIO_LAUNCH_VERIFY") {
            DWORD exitCode = 0;
            if (!GetExitCodeProcess(hProcess.get(), &exitCode) || exitCode != STILL_ACTIVE) {
                std::cerr << "Roblox Studio process is no longer running\n";
                return 1;
            }

            FILETIME ftVerifyCreate, ftVerifyExit, ftVerifyKernel, ftVerifyUser;
            if (!GetProcessTimes(hProcess.get(), &ftVerifyCreate, &ftVerifyExit, &ftVerifyKernel, &ftVerifyUser)) {
                std::cerr << "GetProcessTimes failed during verification: " << GetLastError() << "\n";
                return 1;
            }

            ULARGE_INTEGER verifyUli;
            verifyUli.LowPart = ftVerifyCreate.dwLowDateTime;
            verifyUli.HighPart = ftVerifyCreate.dwHighDateTime;
            if (verifyUli.QuadPart != startedAtFileTime) {
                std::cerr << "Process creation time mismatch during verification\n";
                return 1;
            }

            wchar_t exePath[32768] = { 0 };
            DWORD size = 32768;
            if (!QueryFullProcessImageNameW(hProcess.get(), 0, exePath, &size)) {
                std::cerr << "QueryFullProcessImageNameW failed during verification: " << GetLastError() << "\n";
                return 1;
            }

            if (!paths_equal(exePath, studio_path)) {
                std::cerr << "Process image path mismatch during verification\n";
                return 1;
            }

            std::cout << "CARBON_STUDIO_LAUNCH_VERIFIED" << std::endl;
            continue;
        }

        if (line == "CARBON_STUDIO_LAUNCH_RESUME") {
            if (!hThread.is_valid()) {
                std::cerr << "Thread handle is invalid for resume\n";
                return 1;
            }
            if (ResumeThread(hThread.get()) == static_cast<DWORD>(-1)) {
                std::cerr << "ResumeThread failed: " << GetLastError() << "\n";
                return 1;
            }
            hThread.close();
            std::cout << "CARBON_STUDIO_LAUNCH_RESUMED" << std::endl;
            continue;
        }

        if (line == "CARBON_STUDIO_LAUNCH_COMPLETE") {
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION info = { 0 };
            if (!SetInformationJobObject(hJob.get(), JobObjectExtendedLimitInformation, &info, sizeof(info))) {
                std::cerr << "Failed to clear kill-on-close limit on job object: " << GetLastError() << "\n";
                completed = false;
                return 1;
            }
            completed = true;
            return 0;
        }

        if (line == "CARBON_STUDIO_LAUNCH_ABORT") {
            completed = false;
            return 0;
        }

        std::cerr << "Unknown launch command: " << line << "\n";
        return 1;
    }

    std::cerr << "Unexpected EOF reading stdin command\n";
    return 1;
}

static int cmd_inject(
    const std::string& pid_str,
    const std::string& filetime_str,
    const std::string& loader_b64,
    const std::string& studio_b64
) {
    uint32_t parsed_pid = 0;
    uint64_t expected_filetime = 0;
    if (!parse_uint32(pid_str, parsed_pid)) {
        std::cerr << "Invalid PID argument\n";
        return 1;
    }
    DWORD pid = static_cast<DWORD>(parsed_pid);
    if (!parse_uint64(filetime_str, expected_filetime)) {
        std::cerr << "Invalid FILETIME argument\n";
        return 1;
    }

    std::wstring loader_path;
    if (!decode_base64_utf8(loader_b64, loader_path) || loader_path.empty()) {
        std::cerr << "Invalid base64/UTF-8 loader path\n";
        return 1;
    }

    std::wstring studio_path;
    if (!decode_base64_utf8(studio_b64, studio_path) || studio_path.empty()) {
        std::cerr << "Invalid base64/UTF-8 studio path\n";
        return 1;
    }

    SafeHandle hProcess(OpenProcess(
        PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ,
        FALSE,
        pid
    ));
    if (!hProcess) {
        std::cerr << "OpenProcess failed: " << GetLastError() << "\n";
        return 1;
    }

    FILETIME ftCreate, ftExit, ftKernel, ftUser;
    if (!GetProcessTimes(hProcess.get(), &ftCreate, &ftExit, &ftKernel, &ftUser)) {
        std::cerr << "GetProcessTimes failed: " << GetLastError() << "\n";
        return 1;
    }

    ULARGE_INTEGER uli;
    uli.LowPart = ftCreate.dwLowDateTime;
    uli.HighPart = ftCreate.dwHighDateTime;
    if (uli.QuadPart != expected_filetime) {
        std::cerr << "Process creation time mismatch\n";
        return 1;
    }

    wchar_t exePath[32768] = { 0 };
    DWORD size = 32768;
    if (!QueryFullProcessImageNameW(hProcess.get(), 0, exePath, &size)) {
        std::cerr << "QueryFullProcessImageNameW failed: " << GetLastError() << "\n";
        return 1;
    }

    if (!paths_equal(exePath, studio_path)) {
        std::cerr << "Process image path mismatch\n";
        return 1;
    }

    SIZE_T memSize = (loader_path.length() + 1) * sizeof(wchar_t);
    LPVOID remoteMem = VirtualAllocEx(hProcess.get(), NULL, memSize, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!remoteMem) {
        std::cerr << "VirtualAllocEx failed: " << GetLastError() << "\n";
        return 1;
    }

    SIZE_T bytesWritten = 0;
    if (!WriteProcessMemory(hProcess.get(), remoteMem, loader_path.c_str(), memSize, &bytesWritten) || bytesWritten != memSize) {
        std::cerr << "WriteProcessMemory failed: " << GetLastError() << "\n";
        VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
        return 1;
    }

    HMODULE hKernel32 = GetModuleHandleW(L"kernel32.dll");
    if (!hKernel32) {
        std::cerr << "GetModuleHandleW(kernel32.dll) failed\n";
        VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
        return 1;
    }

    FARPROC pLoadLibraryW = GetProcAddress(hKernel32, "LoadLibraryW");
    if (!pLoadLibraryW) {
        std::cerr << "GetProcAddress(LoadLibraryW) failed\n";
        VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
        return 1;
    }

    DWORD threadId = 0;
    SafeHandle hThread(CreateRemoteThread(
        hProcess.get(),
        NULL,
        0,
        reinterpret_cast<LPTHREAD_START_ROUTINE>(pLoadLibraryW),
        remoteMem,
        CREATE_SUSPENDED,
        &threadId
    ));

    if (!hThread) {
        std::cerr << "CreateRemoteThread failed: " << GetLastError() << "\n";
        VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
        return 1;
    }

    std::cout << "CARBON_RML_INJECTOR_READY" << std::endl;

    std::string line;
    if (!std::getline(std::cin, line)) {
        std::cerr << "Failed to read injector authorization\n";
        return 1;
    }
    if (!line.empty() && line.back() == '\r') line.pop_back();

    if (line != "CARBON_RML_INJECTOR_PROCEED") {
        std::cerr << "Injector authorization mismatch: " << line << "\n";
        return 1;
    }

    if (ResumeThread(hThread.get()) == static_cast<DWORD>(-1)) {
        std::cerr << "ResumeThread failed: " << GetLastError() << "\n";
        return 1;
    }

    std::cout << "CARBON_RML_INJECTOR_STARTED" << std::endl;

    DWORD waitResult = WaitForSingleObject(hThread.get(), 30000);
    bool finished = (waitResult == WAIT_OBJECT_0);

    if (finished) {
        VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
        return 0;
    }

    std::cerr << "WaitForSingleObject failed or timed out: " << waitResult << "\n";
    return 1;
}
static int cmd_terminate(
    const std::string& pid_str,
    const std::string& filetime_str,
    const std::string& studio_b64
) {
    uint32_t parsed_pid = 0;
    uint64_t expected_filetime = 0;
    if (!parse_uint32(pid_str, parsed_pid)) {
        std::cerr << "Invalid PID argument\n";
        return 1;
    }
    DWORD pid = static_cast<DWORD>(parsed_pid);
    if (!parse_uint64(filetime_str, expected_filetime)) {
        std::cerr << "Invalid FILETIME argument\n";
        return 1;
    }

    std::wstring studio_path;
    if (!decode_base64_utf8(studio_b64, studio_path) || studio_path.empty()) {
        std::cerr << "Invalid base64/UTF-8 studio path\n";
        return 1;
    }

    SafeHandle hProcess(OpenProcess(
        PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION | SYNCHRONIZE,
        FALSE,
        pid
    ));
    if (!hProcess) {
        DWORD err = GetLastError();
        if (err == ERROR_INVALID_PARAMETER || err == ERROR_PROC_NOT_FOUND) {
            return 0;
        }
        std::cerr << "OpenProcess failed: " << err << "\n";
        return 1;
    }

    DWORD exitCode = 0;
    if (GetExitCodeProcess(hProcess.get(), &exitCode) && exitCode != STILL_ACTIVE) {
        return 0;
    }

    FILETIME ftCreate, ftExit, ftKernel, ftUser;
    if (!GetProcessTimes(hProcess.get(), &ftCreate, &ftExit, &ftKernel, &ftUser)) {
        std::cerr << "GetProcessTimes failed: " << GetLastError() << "\n";
        return 1;
    }

    ULARGE_INTEGER uli;
    uli.LowPart = ftCreate.dwLowDateTime;
    uli.HighPart = ftCreate.dwHighDateTime;
    if (uli.QuadPart != expected_filetime) {
        return 0;
    }

    wchar_t exePath[32768] = { 0 };
    DWORD size = 32768;
    if (!QueryFullProcessImageNameW(hProcess.get(), 0, exePath, &size)) {
        std::cerr << "QueryFullProcessImageNameW failed: " << GetLastError() << "\n";
        return 1;
    }

    if (!paths_equal(exePath, studio_path)) {
        std::cerr << "Process image path mismatch\n";
        return 1;
    }

    if (!TerminateProcess(hProcess.get(), 1)) {
        DWORD err = GetLastError();
        if (GetExitCodeProcess(hProcess.get(), &exitCode) && exitCode != STILL_ACTIVE) {
            return 0;
        }
        std::cerr << "TerminateProcess failed: " << err << "\n";
        return 1;
    }

    DWORD waitRes = WaitForSingleObject(hProcess.get(), 30000);
    if (waitRes != WAIT_OBJECT_0) {
        if (waitRes == WAIT_TIMEOUT) {
            std::cerr << "Managed Roblox Studio process termination timed out\n";
        } else {
            std::cerr << "WaitForSingleObject failed after termination: " << GetLastError() << "\n";
        }
        return 1;
    }
    return 0;
}

int main(int argc, char* argv[]) {
    if (argc < 2) {
        std::cerr << "Usage: carbon-studio-helper <launch|inject|terminate> [args...]\n";
        return 1;
    }

    std::string cmd = argv[1];
    if (cmd == "launch") {
        if (argc != 8) {
            std::cerr << "Usage: carbon-studio-helper launch <studio_b64> <place_b64> <managed_0_or_1> "
                         "<loader_b64> <build_b64> <dotnet_root_b64>\n";
            return 1;
        }
        return cmd_launch(argv[2], argv[3], argv[4], argv[5], argv[6], argv[7]);
    } else if (cmd == "inject") {
        if (argc != 6) {
            std::cerr << "Usage: carbon-studio-helper inject <pid> <filetime> <loader_b64> <studio_b64>\n";
            return 1;
        }
        return cmd_inject(argv[2], argv[3], argv[4], argv[5]);
    } else if (cmd == "terminate") {
        if (argc != 5) {
            std::cerr << "Usage: carbon-studio-helper terminate <pid> <filetime> <studio_b64>\n";
            return 1;
        }
        return cmd_terminate(argv[2], argv[3], argv[4]);
    }

    std::cerr << "Unknown command: " << cmd << "\n";
    return 1;
}
