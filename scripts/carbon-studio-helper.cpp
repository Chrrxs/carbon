#ifndef UNICODE
#define UNICODE
#endif
#ifndef _UNICODE
#define _UNICODE
#endif

#include <windows.h>
#include <tlhelp32.h>
#include <winhttp.h>
#include <io.h>
#include <fcntl.h>
#include <iostream>
#include <string>
#include <vector>
#include <cstdint>
#include <climits>
#include <algorithm>

class SafeWinHttpHandle {
    HINTERNET handle_;
public:
    explicit SafeWinHttpHandle(HINTERNET h = NULL) : handle_(h) {}
    ~SafeWinHttpHandle() {
        close();
    }
    SafeWinHttpHandle(const SafeWinHttpHandle&) = delete;
    SafeWinHttpHandle& operator=(const SafeWinHttpHandle&) = delete;
    SafeWinHttpHandle(SafeWinHttpHandle&& other) noexcept : handle_(other.handle_) {
        other.handle_ = NULL;
    }
    SafeWinHttpHandle& operator=(SafeWinHttpHandle&& other) noexcept {
        if (this != &other) {
            close();
            handle_ = other.handle_;
            other.handle_ = NULL;
        }
        return *this;
    }
    HINTERNET get() const { return handle_; }
    void reset(HINTERNET h = NULL) {
        close();
        handle_ = h;
    }
    void close() {
        if (handle_) {
            WinHttpCloseHandle(handle_);
            handle_ = NULL;
        }
    }
    bool is_valid() const { return handle_ != NULL; }
    operator bool() const { return is_valid(); }
};

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
static bool parse_uint64_allow_zero(const std::string& str, uint64_t& out) {
    if (str.empty()) return false;
    for (char c : str) {
        if (c < '0' || c > '9') return false;
    }
    try {
        size_t idx = 0;
        unsigned long long val = std::stoull(str, &idx, 10);
        if (idx != str.length()) {
            return false;
        }
        out = static_cast<uint64_t>(val);
        return true;
    } catch (...) {
        return false;
    }
}

static bool read_exact(HANDLE hFile, void* buffer, DWORD bytesToRead) {
    BYTE* ptr = reinterpret_cast<BYTE*>(buffer);
    DWORD totalRead = 0;
    while (totalRead < bytesToRead) {
        DWORD read = 0;
        if (!ReadFile(hFile, ptr + totalRead, bytesToRead - totalRead, &read, NULL) || read == 0) {
            return false;
        }
        totalRead += read;
    }
    return true;
}

static bool write_exact(HANDLE hFile, const void* buffer, DWORD bytesToWrite) {
    const BYTE* ptr = reinterpret_cast<const BYTE*>(buffer);
    DWORD totalWritten = 0;
    while (totalWritten < bytesToWrite) {
        DWORD written = 0;
        if (!WriteFile(hFile, ptr + totalWritten, bytesToWrite - totalWritten, &written, NULL) || written == 0) {
            return false;
        }
        totalWritten += written;
    }
    return true;
}

static bool decode_base64(const std::string& input, std::vector<uint8_t>& out);
static bool utf8_to_wstring(const std::vector<uint8_t>& bytes, std::wstring& out);

static int cmd_bridge_request(
    const std::string& port_str,
    const std::string& method,
    const std::string& path_b64,
    const std::string& content_type_str,
    const std::string& range_str,
    const std::string& timeout_str
) {
    uint32_t port_val = 0;
    if (!parse_uint32(port_str, port_val) || port_val < 1 || port_val > 65535) {
        std::cerr << "Invalid port\n";
        return 1;
    }

    if (method != "GET" && method != "POST" && method != "DELETE") {
        std::cerr << "Invalid HTTP method\n";
        return 1;
    }

    std::vector<uint8_t> path_bytes;
    if (!decode_base64(path_b64, path_bytes) || path_bytes.empty()) {
        std::cerr << "Invalid path base64\n";
        return 1;
    }

    if (path_bytes[0] != '/') {
        std::cerr << "Path must begin with '/'\n";
        return 1;
    }

    if (path_bytes.size() >= 2 && path_bytes[1] == '/') {
        std::cerr << "Path must not supply a host\n";
        return 1;
    }

    for (uint8_t b : path_bytes) {
        if (b == '\r' || b == '\n' || b == '\0') {
            std::cerr << "Path must not contain CR, LF, or NUL\n";
            return 1;
        }
    }

    std::wstring wpath;
    if (!utf8_to_wstring(path_bytes, wpath)) {
        std::cerr << "Path is not valid UTF-8\n";
        return 1;
    }

    std::wstring wcontent_type;
    bool send_content_type = false;
    if (content_type_str == "-") {
        send_content_type = false;
    } else if (content_type_str == "application/json") {
        send_content_type = true;
        wcontent_type = L"application/json";
    } else if (content_type_str == "application/octet-stream") {
        send_content_type = true;
        wcontent_type = L"application/octet-stream";
    } else {
        std::cerr << "Invalid content type\n";
        return 1;
    }

    std::wstring wrange;
    bool send_range = false;
    if (range_str == "-") {
        send_range = false;
    } else {
        if (range_str.rfind("bytes=", 0) != 0) {
            std::cerr << "Invalid range specifier\n";
            return 1;
        }
        std::string spec = range_str.substr(6);
        size_t dash_pos = spec.find('-');
        if (dash_pos == std::string::npos || dash_pos == 0 || dash_pos == spec.length() - 1) {
            std::cerr << "Invalid range specifier format\n";
            return 1;
        }
        std::string start_str = spec.substr(0, dash_pos);
        std::string end_str = spec.substr(dash_pos + 1);
        uint64_t start_val = 0, end_val = 0;
        if (!parse_uint64_allow_zero(start_str, start_val) || !parse_uint64_allow_zero(end_str, end_val) || start_val > end_val) {
            std::cerr << "Invalid range byte bounds\n";
            return 1;
        }
        send_range = true;
        wrange = std::wstring(range_str.begin(), range_str.end());
    }

    uint32_t timeout_val = 0;
    if (!parse_uint32(timeout_str, timeout_val) || timeout_val == 0) {
        std::cerr << "Invalid timeout\n";
        return 1;
    }

    _setmode(_fileno(stdin), _O_BINARY);
    _setmode(_fileno(stdout), _O_BINARY);

    HANDLE hStdin = GetStdHandle(STD_INPUT_HANDLE);
    HANDLE hStdout = GetStdHandle(STD_OUTPUT_HANDLE);

    char magic_req[8] = {0};
    if (!read_exact(hStdin, magic_req, 8) || memcmp(magic_req, "CBRQ0001", 8) != 0) {
        std::cerr << "Invalid request magic\n";
        return 1;
    }

    uint8_t token_len_bytes[4] = {0};
    if (!read_exact(hStdin, token_len_bytes, 4)) {
        std::cerr << "Failed to read token length\n";
        return 1;
    }
    uint32_t token_len = static_cast<uint32_t>(token_len_bytes[0]) |
                        (static_cast<uint32_t>(token_len_bytes[1]) << 8) |
                        (static_cast<uint32_t>(token_len_bytes[2]) << 16) |
                        (static_cast<uint32_t>(token_len_bytes[3]) << 24);

    if (token_len == 0 || token_len > 65536) {
        std::cerr << "Invalid token length\n";
        return 1;
    }

    std::vector<uint8_t> token_bytes(token_len);
    if (!read_exact(hStdin, token_bytes.data(), token_len)) {
        std::cerr << "Failed to read token bytes\n";
        return 1;
    }

    std::wstring wtoken;
    if (!utf8_to_wstring(token_bytes, wtoken)) {
        std::cerr << "Token is not valid UTF-8\n";
        return 1;
    }

    std::vector<uint8_t> request_body;
    BYTE chunk[8192];
    DWORD bytes_read = 0;
    const size_t MAX_REQUEST_BODY = 100 * 1024 * 1024;
    while (true) {
        if (!ReadFile(hStdin, chunk, sizeof(chunk), &bytes_read, NULL) || bytes_read == 0) {
            break;
        }
        if (request_body.size() + bytes_read > MAX_REQUEST_BODY) {
            std::cerr << "Request body exceeds maximum allowed size\n";
            return 1;
        }
        request_body.insert(request_body.end(), chunk, chunk + bytes_read);
    }

    SafeWinHttpHandle hSession(WinHttpOpen(
        L"Carbon-Studio-Helper/1.0",
        WINHTTP_ACCESS_TYPE_NO_PROXY,
        WINHTTP_NO_PROXY_NAME,
        WINHTTP_NO_PROXY_BYPASS,
        0
    ));
    if (!hSession) {
        std::cerr << "WinHttpOpen failed: " << GetLastError() << "\n";
        return 1;
    }

    int timeout_int = (timeout_val > static_cast<uint32_t>(INT_MAX)) ? INT_MAX : static_cast<int>(timeout_val);
    if (!WinHttpSetTimeouts(hSession.get(), timeout_int, timeout_int, timeout_int, timeout_int)) {
        std::cerr << "WinHttpSetTimeouts failed: " << GetLastError() << "\n";
        return 1;
    }

    SafeWinHttpHandle hConnect(WinHttpConnect(
        hSession.get(),
        L"127.0.0.1",
        static_cast<INTERNET_PORT>(port_val),
        0
    ));
    if (!hConnect) {
        std::cerr << "WinHttpConnect failed: " << GetLastError() << "\n";
        return 1;
    }

    std::wstring wmethod(method.begin(), method.end());
    SafeWinHttpHandle hRequest(WinHttpOpenRequest(
        hConnect.get(),
        wmethod.c_str(),
        wpath.c_str(),
        NULL,
        WINHTTP_NO_REFERER,
        WINHTTP_DEFAULT_ACCEPT_TYPES,
        0
    ));
    if (!hRequest) {
        std::cerr << "WinHttpOpenRequest failed: " << GetLastError() << "\n";
        return 1;
    }

    std::wstring headers = L"Authorization: Bearer " + wtoken + L"\r\n";
    if (send_content_type) {
        headers += L"Content-Type: " + wcontent_type + L"\r\n";
    }
    if (send_range) {
        headers += L"Range: " + wrange + L"\r\n";
    }

    if (!WinHttpAddRequestHeaders(
            hRequest.get(),
            headers.c_str(),
            static_cast<DWORD>(-1L),
            WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE)) {
        std::cerr << "WinHttpAddRequestHeaders failed: " << GetLastError() << "\n";
        return 1;
    }

    DWORD body_size = static_cast<DWORD>(request_body.size());
    if (!WinHttpSendRequest(
            hRequest.get(),
            WINHTTP_NO_ADDITIONAL_HEADERS,
            0,
            WINHTTP_NO_REQUEST_DATA,
            0,
            body_size,
            0)) {
        std::cerr << "WinHttpSendRequest failed: " << GetLastError() << "\n";
        return 1;
    }

    if (body_size > 0) {
        DWORD total_written = 0;
        while (total_written < body_size) {
            DWORD chunk_to_write = std::min<DWORD>(body_size - total_written, 65536);
            DWORD written = 0;
            if (!WinHttpWriteData(hRequest.get(), request_body.data() + total_written, chunk_to_write, &written) || written == 0) {
                std::cerr << "WinHttpWriteData failed: " << GetLastError() << "\n";
                return 1;
            }
            total_written += written;
        }
    }

    if (!WinHttpReceiveResponse(hRequest.get(), NULL)) {
        std::cerr << "WinHttpReceiveResponse failed: " << GetLastError() << "\n";
        return 1;
    }

    DWORD status_code = 0;
    DWORD status_code_size = sizeof(status_code);
    if (!WinHttpQueryHeaders(
            hRequest.get(),
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            WINHTTP_HEADER_NAME_BY_INDEX,
            &status_code,
            &status_code_size,
            WINHTTP_NO_HEADER_INDEX)) {
        std::cerr << "WinHttpQueryHeaders status code failed: " << GetLastError() << "\n";
        return 1;
    }

    uint64_t declared_content_length = UINT64_MAX;
    wchar_t cl_buf[64] = {0};
    DWORD cl_buf_size = sizeof(cl_buf);
    if (WinHttpQueryHeaders(
            hRequest.get(),
            WINHTTP_QUERY_CONTENT_LENGTH,
            WINHTTP_HEADER_NAME_BY_INDEX,
            cl_buf,
            &cl_buf_size,
            WINHTTP_NO_HEADER_INDEX)) {
        wchar_t* endptr = nullptr;
        unsigned long long val = wcstoull(cl_buf, &endptr, 10);
        if (endptr != cl_buf && *endptr == L'\0') {
            declared_content_length = static_cast<uint64_t>(val);
        }
    }

    std::vector<uint8_t> content_range_utf8;
    DWORD cr_buf_size = 0;
    WinHttpQueryHeaders(
        hRequest.get(),
        WINHTTP_QUERY_CUSTOM,
        L"Content-Range",
        WINHTTP_NO_OUTPUT_BUFFER,
        &cr_buf_size,
        WINHTTP_NO_HEADER_INDEX
    );
    if (GetLastError() == ERROR_INSUFFICIENT_BUFFER && cr_buf_size > 0) {
        std::vector<wchar_t> cr_buf(cr_buf_size / sizeof(wchar_t) + 1);
        if (WinHttpQueryHeaders(
                hRequest.get(),
                WINHTTP_QUERY_CUSTOM,
                L"Content-Range",
                cr_buf.data(),
                &cr_buf_size,
                WINHTTP_NO_HEADER_INDEX)) {
            std::wstring cr_wstr(cr_buf.data());
            if (!cr_wstr.empty()) {
                int ulen = WideCharToMultiByte(CP_UTF8, 0, cr_wstr.c_str(), static_cast<int>(cr_wstr.size()), NULL, 0, NULL, NULL);
                if (ulen > 0) {
                    content_range_utf8.resize(ulen);
                    WideCharToMultiByte(CP_UTF8, 0, cr_wstr.c_str(), static_cast<int>(cr_wstr.size()), reinterpret_cast<char*>(content_range_utf8.data()), ulen, NULL, NULL);
                }
            }
        }
    }

    std::vector<uint8_t> frame;
    const char magic_resp[8] = {'C', 'B', 'R', 'S', '0', '0', '0', '1'};
    frame.insert(frame.end(), magic_resp, magic_resp + 8);

    uint32_t status_u32 = static_cast<uint32_t>(status_code);
    frame.push_back(static_cast<uint8_t>(status_u32 & 0xFF));
    frame.push_back(static_cast<uint8_t>((status_u32 >> 8) & 0xFF));
    frame.push_back(static_cast<uint8_t>((status_u32 >> 16) & 0xFF));
    frame.push_back(static_cast<uint8_t>((status_u32 >> 24) & 0xFF));

    uint64_t cl_u64 = declared_content_length;
    for (int i = 0; i < 8; ++i) {
        frame.push_back(static_cast<uint8_t>((cl_u64 >> (i * 8)) & 0xFF));
    }

    uint32_t cr_len = static_cast<uint32_t>(content_range_utf8.size());
    frame.push_back(static_cast<uint8_t>(cr_len & 0xFF));
    frame.push_back(static_cast<uint8_t>((cr_len >> 8) & 0xFF));
    frame.push_back(static_cast<uint8_t>((cr_len >> 16) & 0xFF));
    frame.push_back(static_cast<uint8_t>((cr_len >> 24) & 0xFF));

    if (cr_len > 0) {
        frame.insert(frame.end(), content_range_utf8.begin(), content_range_utf8.end());
    }

    if (!write_exact(hStdout, frame.data(), static_cast<DWORD>(frame.size()))) {
        return 1;
    }

    BYTE resp_buf[8192];
    while (true) {
        DWORD read_bytes = 0;
        if (!WinHttpReadData(hRequest.get(), resp_buf, sizeof(resp_buf), &read_bytes)) {
            return 1;
        }
        if (read_bytes == 0) {
            break;
        }
        if (!write_exact(hStdout, resp_buf, read_bytes)) {
            return 1;
        }
    }

    return 0;
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

static bool process_has_module(const DWORD pid, const std::wstring& module_path, bool& inspected) {
    inspected = false;
    SafeHandle snapshot(CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid));
    if (!snapshot) {
        std::cerr << "CreateToolhelp32Snapshot failed while verifying the Carbon RML loader: "
                  << GetLastError() << "\n";
        return false;
    }

    MODULEENTRY32W module = { sizeof(module) };
    if (!Module32FirstW(snapshot.get(), &module)) {
        std::cerr << "Module32FirstW failed while verifying the Carbon RML loader: "
                  << GetLastError() << "\n";
        return false;
    }
    inspected = true;

    do {
        if (paths_equal(module.szExePath, module_path)) {
            return true;
        }
    } while (Module32NextW(snapshot.get(), &module));

    return false;
}

static int cmd_launch(
    const std::string& studio_b64,
    const std::string& place_b64,
    const std::string& managed_str,
    const std::string& loader_b64,
    const std::string& build_b64,
    const std::string& dotnet_root_b64,
    const std::string& bridge_id_b64
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

    std::wstring bridge_id;
    if (!decode_base64_utf8(bridge_id_b64, bridge_id)
        || bridge_id.length() != 32
        || !std::all_of(bridge_id.begin(), bridge_id.end(), [](wchar_t value) {
            return (value >= L'0' && value <= L'9')
                || (value >= L'a' && value <= L'f')
                || (value >= L'A' && value <= L'F');
        })) {
        std::cerr << "Invalid base64/UTF-8 RML bridge ID\n";
        return 1;
    }

    struct EnvironmentValue {
        const wchar_t* name;
        const std::wstring* value;
    };
    const EnvironmentValue environment[] = {
        { L"CARBON_RML_LOADER", &loader_path },
        { L"CARBON_RML_BUILD_VERSION", &build_version },
        { L"CARBON_RML_BRIDGE_ID", &bridge_id },
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

static bool validate_process_identity(
    const std::string& pid_str,
    const std::string& filetime_str,
    const std::string& studio_b64,
    DWORD desired_access,
    DWORD& out_pid,
    SafeHandle& out_hProcess,
    bool allow_not_running_or_mismatch = false
) {
    uint32_t parsed_pid = 0;
    uint64_t expected_filetime = 0;
    if (!parse_uint32(pid_str, parsed_pid)) {
        std::cerr << "Invalid PID argument\n";
        return false;
    }
    out_pid = static_cast<DWORD>(parsed_pid);
    if (!parse_uint64(filetime_str, expected_filetime)) {
        std::cerr << "Invalid FILETIME argument\n";
        return false;
    }

    std::wstring studio_path;
    if (!decode_base64_utf8(studio_b64, studio_path) || studio_path.empty()) {
        std::cerr << "Invalid base64/UTF-8 studio path\n";
        return false;
    }

    SafeHandle hProcess(OpenProcess(desired_access, FALSE, out_pid));
    if (!hProcess && (desired_access & PROCESS_QUERY_INFORMATION) != 0) {
        hProcess.reset(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, out_pid));
    }
    if (!hProcess) {
        DWORD err = GetLastError();
        if (allow_not_running_or_mismatch && (err == ERROR_INVALID_PARAMETER || err == ERROR_PROC_NOT_FOUND)) {
            return true;
        }
        std::cerr << "OpenProcess failed: " << err << "\n";
        return false;
    }

    DWORD exitCode = 0;
    if (GetExitCodeProcess(hProcess.get(), &exitCode) && exitCode != STILL_ACTIVE) {
        if (allow_not_running_or_mismatch) {
            return true;
        }
        std::cerr << "Process is not running\n";
        return false;
    }

    FILETIME ftCreate, ftExit, ftKernel, ftUser;
    if (!GetProcessTimes(hProcess.get(), &ftCreate, &ftExit, &ftKernel, &ftUser)) {
        std::cerr << "GetProcessTimes failed: " << GetLastError() << "\n";
        return false;
    }

    ULARGE_INTEGER uli;
    uli.LowPart = ftCreate.dwLowDateTime;
    uli.HighPart = ftCreate.dwHighDateTime;
    if (uli.QuadPart != expected_filetime) {
        if (allow_not_running_or_mismatch) {
            return true;
        }
        std::cerr << "Process creation time mismatch\n";
        return false;
    }

    wchar_t exePath[32768] = { 0 };
    DWORD size = 32768;
    if (!QueryFullProcessImageNameW(hProcess.get(), 0, exePath, &size)) {
        std::cerr << "QueryFullProcessImageNameW failed: " << GetLastError() << "\n";
        return false;
    }

    if (!paths_equal(exePath, studio_path)) {
        std::cerr << "Process image path mismatch\n";
        return false;
    }

    out_hProcess = std::move(hProcess);
    return true;
}

static int cmd_inject(
    const std::string& pid_str,
    const std::string& filetime_str,
    const std::string& loader_b64,
    const std::string& studio_b64
) {
    std::wstring loader_path;
    if (!decode_base64_utf8(loader_b64, loader_path) || loader_path.empty()) {
        std::cerr << "Invalid base64/UTF-8 loader path\n";
        return 1;
    }

    DWORD pid = 0;
    SafeHandle hProcess;
    if (!validate_process_identity(
            pid_str,
            filetime_str,
            studio_b64,
            PROCESS_CREATE_THREAD | PROCESS_QUERY_INFORMATION | PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ,
            pid,
            hProcess
        )) {
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

    // LoadLibrary may remain inside the loader's initialization while RML waits
    // for Studio's resumed engine thread. The bridge attestation performed by
    // Carbon is the authoritative readiness gate; this wait is only an
    // opportunity to reclaim the remote path allocation on the fast path.
    DWORD waitResult = WaitForSingleObject(hThread.get(), 1000);
    if (waitResult == WAIT_OBJECT_0) {
        DWORD remoteResult = 0;
        if (!GetExitCodeThread(hThread.get(), &remoteResult)) {
            std::cerr << "GetExitCodeThread failed: " << GetLastError() << "\n";
            VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
            return 1;
        }
        if (remoteResult == 0) {
            bool modulesInspected = false;
            const bool moduleLoaded = process_has_module(pid, loader_path, modulesInspected);
            if (!modulesInspected) {
                VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
                return 1;
            }
            if (!moduleLoaded) {
                std::cerr << "LoadLibraryW returned null for the Carbon RML loader\n";
                VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
                return 1;
            }
        }
        VirtualFreeEx(hProcess.get(), remoteMem, 0, MEM_RELEASE);
        return 0;
    }
    if (waitResult == WAIT_TIMEOUT) {
        // The remote thread still owns remoteMem. Keep it allocated until the
        // Studio process exits rather than freeing memory beneath LoadLibrary.
        return 0;
    }

    std::cerr << "WaitForSingleObject failed: " << GetLastError() << "\n";
    return 1;
}

static int cmd_terminate(
    const std::string& pid_str,
    const std::string& filetime_str,
    const std::string& studio_b64
) {
    DWORD pid = 0;
    SafeHandle hProcess;
    if (!validate_process_identity(
            pid_str,
            filetime_str,
            studio_b64,
            PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION | SYNCHRONIZE,
            pid,
            hProcess,
            true
        )) {
        return 1;
    }

    if (!hProcess.is_valid()) {
        return 0;
    }

    DWORD exitCode = 0;
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
struct FocusWindowCandidate {
    HWND hwnd;
    LONG area;
};

struct FocusEnumContext {
    DWORD target_pid;
    bool allow_tool_window;
    std::vector<FocusWindowCandidate> candidates;
};

static BOOL CALLBACK enum_windows_callback(HWND hwnd, LPARAM lParam) {
    FocusEnumContext* ctx = reinterpret_cast<FocusEnumContext*>(lParam);
    if (!ctx) return FALSE;

    DWORD window_pid = 0;
    GetWindowThreadProcessId(hwnd, &window_pid);
    if (window_pid != ctx->target_pid) {
        return TRUE;
    }

    if (!IsWindowVisible(hwnd)) {
        return TRUE;
    }

    if (GetWindow(hwnd, GW_OWNER) != NULL) {
        return TRUE;
    }

    LONG_PTR style = GetWindowLongPtrW(hwnd, GWL_STYLE);
    if ((style & WS_CHILD) != 0) {
        return TRUE;
    }

    if (!ctx->allow_tool_window) {
        LONG_PTR ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ((ex_style & WS_EX_TOOLWINDOW) != 0) {
            return TRUE;
        }
    }

    RECT r = { 0, 0, 0, 0 };
    LONG area = 0;
    if (GetWindowRect(hwnd, &r)) {
        LONG width = r.right - r.left;
        LONG height = r.bottom - r.top;
        if (width > 0 && height > 0) {
            area = width * height;
        }
    }

    FocusWindowCandidate candidate;
    candidate.hwnd = hwnd;
    candidate.area = area;
    ctx->candidates.push_back(candidate);

    return TRUE;
}

static HWND focus_target(HWND root_hwnd, DWORD process_id) {
    HWND target_hwnd = root_hwnd;
    for (int depth = 0; depth < 16; ++depth) {
        HWND popup_hwnd = GetLastActivePopup(target_hwnd);
        if (popup_hwnd == NULL || popup_hwnd == target_hwnd || !IsWindowVisible(popup_hwnd)) {
            break;
        }

        DWORD popup_pid = 0;
        GetWindowThreadProcessId(popup_hwnd, &popup_pid);
        if (popup_pid != process_id) {
            break;
        }
        target_hwnd = popup_hwnd;
    }
    return target_hwnd;
}

static bool activate_window(HWND target_hwnd) {
    SetForegroundWindow(target_hwnd);

    if (GetForegroundWindow() != target_hwnd) {
        DWORD current_thread = GetCurrentThreadId();
        DWORD target_thread = GetWindowThreadProcessId(target_hwnd, NULL);
        HWND foreground_hwnd = GetForegroundWindow();
        DWORD foreground_thread = foreground_hwnd
            ? GetWindowThreadProcessId(foreground_hwnd, NULL)
            : 0;

        bool attached_foreground =
            foreground_thread != 0 &&
            foreground_thread != current_thread &&
            AttachThreadInput(current_thread, foreground_thread, TRUE);
        bool attached_target =
            target_thread != 0 &&
            target_thread != current_thread &&
            target_thread != foreground_thread &&
            AttachThreadInput(current_thread, target_thread, TRUE);

        BringWindowToTop(target_hwnd);
        SetForegroundWindow(target_hwnd);
        SetFocus(target_hwnd);

        if (attached_target) {
            AttachThreadInput(current_thread, target_thread, FALSE);
        }
        if (attached_foreground) {
            AttachThreadInput(current_thread, foreground_thread, FALSE);
        }
    }

    for (int attempt = 0; attempt < 10 && GetForegroundWindow() != target_hwnd; ++attempt) {
        Sleep(10);
    }
    return GetForegroundWindow() == target_hwnd;
}

static int cmd_focus(
    const std::string& pid_str,
    const std::string& filetime_str,
    const std::string& studio_b64,
    const std::string& restore_str
) {
    if (restore_str != "0" && restore_str != "1") {
        std::cerr << "Invalid restore flag (must be 0 or 1)\n";
        return 1;
    }
    bool restore_previous = (restore_str == "1");

    DWORD pid = 0;
    SafeHandle hProcess;
    if (!validate_process_identity(
            pid_str,
            filetime_str,
            studio_b64,
            PROCESS_QUERY_INFORMATION,
            pid,
            hProcess,
            false
        )) {
        return 1;
    }

    FocusEnumContext ctx;
    ctx.target_pid = pid;
    ctx.allow_tool_window = false;
    EnumWindows(enum_windows_callback, reinterpret_cast<LPARAM>(&ctx));

    if (ctx.candidates.empty()) {
        ctx.allow_tool_window = true;
        EnumWindows(enum_windows_callback, reinterpret_cast<LPARAM>(&ctx));
    }

    if (ctx.candidates.empty()) {
        std::cerr << "Roblox Studio process " << pid << " has no main window\n";
        return 1;
    }

    std::stable_sort(ctx.candidates.begin(), ctx.candidates.end(),
        [](const FocusWindowCandidate& a, const FocusWindowCandidate& b) {
            return a.area > b.area;
        });

    HWND root_hwnd = ctx.candidates.front().hwnd;
    HWND target_hwnd = focus_target(root_hwnd, pid);
    HWND previous_hwnd = restore_previous ? GetForegroundWindow() : NULL;

    if (IsIconic(target_hwnd)) {
        ShowWindowAsync(target_hwnd, SW_RESTORE);
    }
    if (!activate_window(target_hwnd)) {
        std::cerr << "Windows denied foreground activation for Roblox Studio process " << pid << "\n";
        return 1;
    }

    if (restore_previous && previous_hwnd != NULL && previous_hwnd != target_hwnd && IsWindow(previous_hwnd)) {
        if (!activate_window(previous_hwnd)) {
            std::cerr << "Windows denied restoration of the previously focused window\n";
            return 1;
        }
    }

    return 0;
}

int main(int argc, char* argv[]) {
    if (argc < 2) {
        std::cerr << "Usage: carbon-studio-helper <launch|inject|terminate|focus|bridge-request> [args...]\n";
        return 1;
    }

    std::string cmd = argv[1];
    if (cmd == "launch") {
        if (argc != 9) {
            std::cerr << "Usage: carbon-studio-helper launch <studio_b64> <place_b64> <managed_0_or_1> "
                         "<loader_b64> <build_b64> <dotnet_root_b64> <bridge_id_b64>\n";
            return 1;
        }
        return cmd_launch(argv[2], argv[3], argv[4], argv[5], argv[6], argv[7], argv[8]);
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
    } else if (cmd == "focus") {
        if (argc != 6) {
            std::cerr << "Usage: carbon-studio-helper focus <pid> <filetime> <studio_b64> <restore_0_or_1>\n";
            return 1;
        }
        return cmd_focus(argv[2], argv[3], argv[4], argv[5]);
    } else if (cmd == "bridge-request") {
        if (argc != 8) {
            std::cerr << "Usage: carbon-studio-helper bridge-request <port> <method> <path_b64> <content_type_or_dash> <range_or_dash> <timeout_ms>\n";
            return 1;
        }
        return cmd_bridge_request(argv[2], argv[3], argv[4], argv[5], argv[6], argv[7]);
    }
    std::cerr << "Unknown command: " << cmd << "\n";
    return 1;
}
