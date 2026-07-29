#include <filesystem>
#include <format>
#include <fstream>
#include <iostream>
#include <set>
#include <sstream>
#include <string>
#include <vector>

#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <ImageHlp.h>

namespace fs = std::filesystem;

using std::cerr;
using std::cout;
using std::endl;
using std::ifstream;
using std::ofstream;
using std::string;

struct ExportFunction {
    uint16_t ordinal;
    bool is_named;
    string name;

    ExportFunction(uint16_t ordinal, bool is_named, string name)
        : ordinal(ordinal), is_named(is_named), name(name) {
    }
};

std::vector<ExportFunction> dump_exports(const fs::path &dll_path) {
    HANDLE dll_file = CreateFileW(dll_path.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr,
                                  OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (dll_file == INVALID_HANDLE_VALUE) {
        auto err_msg = std::format("Failed to open DLL file: {}", GetLastError());
        cerr << err_msg << '\n';
        return {};
    }

    HANDLE dll_mapping = CreateFileMappingW(dll_file, nullptr, PAGE_READONLY, 0, 0, nullptr);
    if (!dll_mapping) {
        CloseHandle(dll_file);
        auto err_msg = std::format("Failed to create file mapping: {}", GetLastError());
        cerr << err_msg << '\n';
        return {};
    }

    void *dll_base = MapViewOfFile(dll_mapping, FILE_MAP_READ, 0, 0, 0);
    if (!dll_base) {
        CloseHandle(dll_mapping);
        CloseHandle(dll_file);
        auto err_msg = std::format("Failed to map view of file: {}", GetLastError());
        cerr << err_msg << '\n';
        return {};
    }

    ULONG export_directory_size = 0;
    auto export_directory =
            static_cast<IMAGE_EXPORT_DIRECTORY *>(ImageDirectoryEntryToData(
                dll_base, FALSE, IMAGE_DIRECTORY_ENTRY_EXPORT,
                &export_directory_size));

    if (export_directory == nullptr) {
        auto err_msg = std::format("Failed to get export directory: {}", GetLastError());
        cerr << err_msg << '\n';
        UnmapViewOfFile(dll_base);
        CloseHandle(dll_mapping);
        CloseHandle(dll_file);
        return {};
    }

    auto dos_header = static_cast<IMAGE_DOS_HEADER *>(dll_base);
    auto nt_header = reinterpret_cast<IMAGE_NT_HEADERS *>(
        static_cast<char *>(dll_base) + dos_header->e_lfanew);

    auto name_rvas = static_cast<DWORD *>(ImageRvaToVa(nt_header, dll_base, export_directory->AddressOfNames, nullptr));
    auto function_rvas = static_cast<DWORD *>(ImageRvaToVa(nt_header, dll_base, export_directory->AddressOfFunctions,
                                                           nullptr));
    auto ordinals = static_cast<uint16_t *>(ImageRvaToVa(nt_header, dll_base, export_directory->AddressOfNameOrdinals,
                                                         nullptr));

    std::vector<ExportFunction> exports;
    std::set<uint16_t> exported_ordinals;

    // Process named exports
    for (size_t i = 0; i < export_directory->NumberOfNames; i++) {
        std::string export_name = static_cast<char *>(ImageRvaToVa(nt_header, dll_base, name_rvas[i], NULL));
        uint16_t ordinal = ordinals[i] + export_directory->Base;

        ExportFunction named_export(ordinal, true, export_name);
        exports.push_back(named_export);
        exported_ordinals.insert(ordinal);
    }

    // Process ordinal-only exports
    for (size_t i = 0; i < export_directory->NumberOfFunctions; i++) {
        uint16_t ordinal = static_cast<uint16_t>(export_directory->Base + i);

        if (uint32_t function_rva = function_rvas[i]; function_rva == 0) continue;
        if (exported_ordinals.contains(ordinal)) continue; // Already exported by name

        ExportFunction ordinal_export(ordinal, false, std::format("ordinal{}", ordinal));
        exports.push_back(ordinal_export);
    }

    UnmapViewOfFile(dll_base);
    CloseHandle(dll_mapping);
    CloseHandle(dll_file);

    return exports;
}

std::vector<ExportFunction> read_exports_file(const fs::path &exp_path, fs::path &dll_path_out) {
    ifstream file(exp_path);
    string line;
    std::vector<ExportFunction> exports;

    // Read the path from the first line
    if (std::getline(file, line) && line.starts_with("Path: ")) {
        dll_path_out = line.substr(6); // Remove "Path: " prefix
    }

    // Skip empty line
    std::getline(file, line);

    // Read exports
    while (std::getline(file, line)) {
        if (line.empty()) continue;

        std::istringstream iss(line);
        uint16_t ordinal;
        string export_name;

        iss >> ordinal;
        bool is_named = iss >> export_name && !export_name.empty();

        ExportFunction exp(ordinal, is_named, is_named ? export_name : std::format("ordinal{}", ordinal));
        exports.push_back(exp);
    }

    return exports;
}

int main(int argc, char *argv[]) {
    if (argc != 3) {
        cerr << "Usage: proxy_generator.exe <input_dll_or_exports_file> <output_directory>" << endl;
        cerr << "  input_dll_or_exports_file: Path to DLL file or .exports file" << endl;
        cerr << "  output_directory: Directory where proxy files will be generated" << endl;
        return -1;
    }

    const fs::path input_file = argv[1];
    const fs::path output_path = argv[2];

    if (!fs::exists(input_file)) {
        cerr << "Input file doesn't exist: " << input_file << endl;
        return -1;
    }

    fs::path input_dll = input_file;
    fs::path input_dll_name = input_file.filename();
    std::vector<ExportFunction> exports;

    if (input_file.extension() == ".exports") {
        cout << std::format("Generating proxy using exports file: {}", input_dll_name.string()) << endl;
        exports = read_exports_file(input_file, input_dll);
        input_dll_name = input_dll.filename();
    } else {
        cout << std::format("Generating proxy for DLL: {}", input_dll.string()) << endl;
        exports = dump_exports(input_dll);

        const auto exports_path = (output_path / input_dll_name).replace_extension("exports");
        fs::create_directories(output_path);
        ofstream exports_file(exports_path);
        exports_file << "Path: " << input_dll.string() << endl << endl;
        for (size_t i = 0; i < exports.size(); ++i) {
            const auto &e = exports[i];
            exports_file << std::format("{} {}", e.ordinal, e.is_named ? e.name : "") << endl;
        }
        exports_file.close();
        cout << std::format("Exports file generated: {}", exports_path.string()) << endl;
    }

    if (exports.empty()) {
        cerr << "No exports found in the DLL!" << endl;
        return -1;
    }

    cout << std::format("Found {} exports", exports.size()) << endl;

    fs::create_directories(output_path);

    // Generate .def file
    ofstream def_file((output_path / input_dll_name).replace_extension("def"));
    def_file << std::format("LIBRARY {}", fs::path(input_dll_name).replace_extension().string()) << endl;
    def_file << "EXPORTS" << endl;

    for (size_t i = 0; i < exports.size(); ++i) {
        const auto &e = exports[i];
        def_file << std::format("  {}=proxy_func_{} @{}", e.name, i, e.ordinal) << endl;
    }
    def_file.close();

    // Generate .asm file
    ofstream asm_file((output_path / input_dll_name).replace_extension("asm"));
    asm_file << ".code" << endl;
    asm_file << "extern g_OriginalFunctions:QWORD" << endl;
    asm_file << endl;

    for (size_t i = 0; i < exports.size(); ++i) {
        asm_file << std::format("proxy_func_{} proc", i) << endl;
        asm_file << std::format("  jmp g_OriginalFunctions[8*{}]", i) << endl;
        asm_file << std::format("proxy_func_{} endp", i) << endl;
        asm_file << endl;
    }

    asm_file << "end" << endl;
    asm_file.close();

    ofstream cpp_file(output_path / "dllmain.cpp");
    cpp_file << "/**" << endl;
    cpp_file << " * Roblox ModLoader Proxy" << endl;
    cpp_file << " * Generated by proxy_generator" << endl;
    cpp_file << " * This file is auto-generated. Do not edit manually." << endl;
    cpp_file << " */" << endl;
    cpp_file << endl;
    cpp_file << "#include <Windows.h>" << endl;
    cpp_file << "#include <filesystem>" << endl;
    cpp_file << "#include <string>" << endl;
    cpp_file << "#include <cstdint>" << endl;
    cpp_file << endl;
    cpp_file << "#pragma comment(lib, \"user32.lib\")" << endl;
    cpp_file << endl;
    cpp_file << "namespace fs = std::filesystem;" << endl;
    cpp_file << endl;
    cpp_file << "HMODULE g_original_dll = nullptr;" << endl;
    cpp_file << "HMODULE g_roblox_mod_loader_dll = nullptr;" << endl;
    cpp_file << std::format("extern \"C\" uintptr_t g_OriginalFunctions[{}] = {{0}};", exports.size()) << endl;
    cpp_file << endl;

    cpp_file << "void setup_function_pointers() {" << endl;
    for (size_t i = 0; i < exports.size(); ++i) {
        const auto &e = exports[i];
        string getter = e.is_named ? std::format("\"{}\"", e.name) : std::format("MAKEINTRESOURCEA({})", e.ordinal);
        cpp_file << std::format("    g_OriginalFunctions[{}] = (uintptr_t)GetProcAddress(g_original_dll, {});", i,
                                getter) << endl;
    }
    cpp_file << "}" << endl;
    cpp_file << endl;

    cpp_file << "void load_original_dll() {" << endl;
    cpp_file << "    wchar_t system_path[MAX_PATH];" << endl;
    cpp_file << "    GetSystemDirectoryW(system_path, MAX_PATH);" << endl;
    cpp_file << endl;
    cpp_file << std::format("    const fs::path dll_path = fs::path(system_path) / L\"{}\";",
                            input_dll_name.string()) << endl;
    cpp_file << endl;
    cpp_file << "    g_original_dll = LoadLibraryW(dll_path.c_str());" << endl;
    cpp_file << "    if (!g_original_dll) {" << endl;
    cpp_file <<
            "        MessageBoxW(nullptr, L\"Failed to load original DLL\", L\"Roblox ModLoader Error\", MB_OK | MB_ICONERROR);"
            << endl;
    cpp_file << "        ExitProcess(1);" << endl;
    cpp_file << "    }" << endl;
    cpp_file << "}" << endl;
    cpp_file << endl;

    cpp_file << "std::wstring environment_variable_wide(const wchar_t* name) {" << endl;
    cpp_file << "    const DWORD size = GetEnvironmentVariableW(name, nullptr, 0);" << endl;
    cpp_file << "    if (size == 0) return {};" << endl;
    cpp_file << "    std::wstring value(size, L'\\0');" << endl;
    cpp_file << "    const DWORD written = GetEnvironmentVariableW(name, value.data(), size);" << endl;
    cpp_file << "    if (written == 0 || written >= size) return {};" << endl;
    cpp_file << "    value.resize(written);" << endl;
    cpp_file << "    return value;" << endl;
    cpp_file << "}" << endl;
    cpp_file << endl;

    cpp_file << "std::string environment_variable(const char* name) {" << endl;
    cpp_file << "    const DWORD size = GetEnvironmentVariableA(name, nullptr, 0);" << endl;
    cpp_file << "    if (size == 0) return {};" << endl;
    cpp_file << "    std::string value(size, '\\0');" << endl;
    cpp_file << "    const DWORD written = GetEnvironmentVariableA(name, value.data(), size);" << endl;
    cpp_file << "    if (written == 0 || written >= size) return {};" << endl;
    cpp_file << "    value.resize(written);" << endl;
    cpp_file << "    return value;" << endl;
    cpp_file << "}" << endl;
    cpp_file << endl;

    cpp_file << "bool attest_loaded_build(HMODULE mod_loader, bool require_expected_build) {" << endl;
    cpp_file << "    using BuildVersionFn = const char* (__cdecl*)();" << endl;
    cpp_file << "    const auto build_version = reinterpret_cast<BuildVersionFn>(" << endl;
    cpp_file << "        GetProcAddress(mod_loader, \"carbon_rml_build_version\"));" << endl;
    cpp_file << "    if (!build_version) return false;" << endl;
    cpp_file << "    const char* loaded_build = build_version();" << endl;
    cpp_file << "    if (!loaded_build || loaded_build[0] == '\\0') return false;" << endl;
    cpp_file << "    const std::string expected_build = environment_variable(\"CARBON_RML_BUILD_VERSION\");" << endl;
    cpp_file << "    if (require_expected_build && expected_build.empty()) return false;" << endl;
    cpp_file << "    if (!expected_build.empty() && expected_build != loaded_build) return false;" << endl;
    cpp_file << "    return SetEnvironmentVariableA(\"CARBON_RML_LOADED_BUILD_VERSION\", loaded_build) != FALSE;" << endl;
    cpp_file << "}" << endl;
    cpp_file << endl;

    cpp_file << "HMODULE load_roblox_mod_loader(const std::wstring& isolated_loader) {" << endl;
    cpp_file << "    if (isolated_loader.empty()) return nullptr;" << endl;
    cpp_file << "    const fs::path isolated_path = isolated_loader;" << endl;
    cpp_file << "    if (!isolated_path.is_absolute()) return nullptr;" << endl;
    cpp_file << "    HMODULE mod_loader = LoadLibraryW(isolated_path.c_str());" << endl;
    cpp_file << "    if (!mod_loader) return nullptr;" << endl;
    cpp_file << "    if (!attest_loaded_build(mod_loader, true)) {" << endl;
    cpp_file << "        return nullptr;" << endl;
    cpp_file << "    }" << endl;
    cpp_file << "    return mod_loader;" << endl;
    cpp_file << "}" << endl;
    cpp_file << endl;

    cpp_file << "BOOL WINAPI DllMain(HMODULE hModule, DWORD dwReason, LPVOID lpReserved) {" << endl;
    cpp_file << "    switch (dwReason) {" << endl;
    cpp_file << "    case DLL_PROCESS_ATTACH:" << endl;
    cpp_file << "        load_original_dll();" << endl;
    cpp_file << "        setup_function_pointers();" << endl;
    cpp_file << "        {" << endl;
    cpp_file << "            const std::wstring isolated_loader = environment_variable_wide(L\"CARBON_RML_LOADER\");" << endl;
    cpp_file << "            g_roblox_mod_loader_dll = load_roblox_mod_loader(isolated_loader);" << endl;
    cpp_file << "            if (!isolated_loader.empty() && !g_roblox_mod_loader_dll) {" << endl;
    cpp_file << "                MessageBoxW(nullptr, " << endl;
    cpp_file << "                           L\"Failed to load Carbon's roblox_modloader.dll. Reinstall Carbon or run carbon rml ensure.\"," << endl;
    cpp_file << "                           L\"Carbon RML Error\", MB_OK | MB_ICONERROR);" << endl;
    cpp_file << "                ExitProcess(1);" << endl;
    cpp_file << "            }" << endl;
    cpp_file << "        }" << endl;
    cpp_file << "        break;" << endl;
    cpp_file << endl;
    cpp_file << "    case DLL_PROCESS_DETACH:" << endl;
    cpp_file << "        if (g_original_dll) {" << endl;
    cpp_file << "            FreeLibrary(g_original_dll);" << endl;
    cpp_file << "        }" << endl;
    cpp_file << "        if (g_roblox_mod_loader_dll) {" << endl;
    cpp_file << "            FreeLibrary(g_roblox_mod_loader_dll);" << endl;
    cpp_file << "        }" << endl;
    cpp_file << "        break;" << endl;
    cpp_file << "    }" << endl;
    cpp_file << endl;
    cpp_file << "    return TRUE;" << endl;
    cpp_file << "}" << endl;

    cpp_file.close();

    ofstream rc_file(output_path / "proxy.rc");
    rc_file << "#include <winver.h>" << endl;
    rc_file << endl;
    rc_file << "VS_VERSION_INFO VERSIONINFO" << endl;
    rc_file << "FILEVERSION 1,0,0,0" << endl;
    rc_file << "PRODUCTVERSION 1,0,0,0" << endl;
    rc_file << "FILEFLAGSMASK VS_FFI_FILEFLAGSMASK" << endl;
    rc_file << "FILEFLAGS 0x0L" << endl;
    rc_file << "FILEOS VOS__WINDOWS32" << endl;
    rc_file << "FILETYPE VFT_DLL" << endl;
    rc_file << "FILESUBTYPE VFT2_UNKNOWN" << endl;
    rc_file << "BEGIN" << endl;
    rc_file << "    BLOCK \"StringFileInfo\"" << endl;
    rc_file << "    BEGIN" << endl;
    rc_file << "        BLOCK \"040904b0\"" << endl;
    rc_file << "        BEGIN" << endl;
    rc_file << "            VALUE \"CompanyName\", \"Roblox ModLoader\"" << endl;
    rc_file << "            VALUE \"FileDescription\", \"Roblox ModLoader Proxy DLL\"" << endl;
    rc_file << std::format("            VALUE \"FileVersion\", \"1.0.0.0 ({})\"", input_dll_name.string()) << endl;
    rc_file << std::format("            VALUE \"InternalName\", \"{}\"", input_dll_name.string()) << endl;
    rc_file << "            VALUE \"LegalCopyright\", \"Copyright (C) 2025 Roblox ModLoader\"" << endl;
    rc_file << std::format("            VALUE \"OriginalFilename\", \"{}\"", input_dll_name.string()) << endl;
    rc_file << "            VALUE \"ProductName\", \"Roblox ModLoader\"" << endl;
    rc_file << "            VALUE \"ProductVersion\", \"1.0.0.0\"" << endl;
    rc_file << "        END" << endl;
    rc_file << "    END" << endl;
    rc_file << "    BLOCK \"VarFileInfo\"" << endl;
    rc_file << "    BEGIN" << endl;
    rc_file << "        VALUE \"Translation\", 0x409, 1200" << endl;
    rc_file << "    END" << endl;
    rc_file << "END" << endl;
    rc_file.close();

    cout << "Proxy generation completed successfully!" << endl;
    cout << "Generated files:" << endl;
    cout << "  - " << (output_path / input_dll_name).replace_extension("def").string() << endl;
    cout << "  - " << (output_path / input_dll_name).replace_extension("asm").string() << endl;
    cout << "  - " << (output_path / "dllmain.cpp").string() << endl;
    cout << "  - " << (output_path / "proxy.rc").string() << endl;

    return 0;
}
