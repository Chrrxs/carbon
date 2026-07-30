set(ROBLOX_MODLOADER_SOURCE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/src")
set(ROBLOX_MODLOADER_INCLUDE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/include")

set(MODLOADER_SOURCES
        "${ROBLOX_MODLOADER_SOURCE_DIR}/main.cpp"
        "${ROBLOX_MODLOADER_SOURCE_DIR}/pointers.cpp"
)

file(GLOB_RECURSE MODLOADER_SUBDIRS CONFIGURE_DEPENDS
        "${ROBLOX_MODLOADER_SOURCE_DIR}/**/*.cpp"
        "${ROBLOX_MODLOADER_SOURCE_DIR}/**/*.hpp"
)

if (NOT RML_ENABLE_LUAU)
    list(FILTER MODLOADER_SUBDIRS EXCLUDE REGEX "/luau/")
    list(FILTER MODLOADER_SUBDIRS EXCLUDE REGEX "/hooks/script_context/")
    list(FILTER MODLOADER_SUBDIRS EXCLUDE REGEX "/roblox/jobs/scripting/")
endif ()

list(APPEND MODLOADER_SOURCES ${MODLOADER_SUBDIRS})

file(GLOB_RECURSE MODLOADER_VENDOR_SOURCES CONFIGURE_DEPENDS
        "${RML_THIRDPARTY_G3D_SRC_DIR}/*.cpp"
)
list(APPEND MODLOADER_SOURCES ${MODLOADER_VENDOR_SOURCES})

set(RML_VERSION_SCRIPT "${PROJECT_SOURCE_DIR}/cmake/generate-version.cmake")
set(RML_VERSION_INPUT "${ROBLOX_MODLOADER_SOURCE_DIR}/version.cpp.in")
set(RML_VERSION_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/generated/version.cpp")

find_package(Git QUIET)

execute_process(COMMAND ${CMAKE_COMMAND}
        "-DGIT_EXECUTABLE=${GIT_EXECUTABLE}"
        "-DCARBON_BUILD_VERSION=${CARBON_BUILD_VERSION}"
        "-DVERSION_REPO_DIR=${CARBON_MONOREPO_ROOT}"
        "-DVERSION_INPUT=${RML_VERSION_INPUT}"
        "-DVERSION_OUTPUT=${RML_VERSION_OUTPUT}"
        -P "${RML_VERSION_SCRIPT}"
)
set_source_files_properties("${RML_VERSION_OUTPUT}" PROPERTIES GENERATED TRUE)
list(APPEND MODLOADER_SOURCES "${RML_VERSION_OUTPUT}")

add_custom_target(roblox_modloader_version
        COMMAND ${CMAKE_COMMAND}
        "-DGIT_EXECUTABLE=${GIT_EXECUTABLE}"
        "-DCARBON_BUILD_VERSION=${CARBON_BUILD_VERSION}"
        "-DVERSION_REPO_DIR=${CARBON_MONOREPO_ROOT}"
        "-DVERSION_INPUT=${RML_VERSION_INPUT}"
        "-DVERSION_OUTPUT=${RML_VERSION_OUTPUT}"
        -P "${RML_VERSION_SCRIPT}"
        BYPRODUCTS "${RML_VERSION_OUTPUT}"
        COMMENT "Updating version information from git"
        VERBATIM
)

add_library(roblox_modloader SHARED ${MODLOADER_SOURCES})

add_dependencies(roblox_modloader roblox_modloader_version)

setup_compiler_flags(roblox_modloader)
setup_compile_definitions(roblox_modloader PRIVATE)
target_compile_definitions(roblox_modloader PRIVATE RML_ENABLE_LUAU=$<BOOL:${RML_ENABLE_LUAU}>)
setup_core_dependencies(roblox_modloader PRIVATE)
setup_luau_dependencies(roblox_modloader PRIVATE)
setup_dotnet_hosting(roblox_modloader PRIVATE)

target_link_libraries(roblox_modloader PRIVATE rml_thirdparty)

if (UNIX AND NOT APPLE)
    target_link_libraries(roblox_modloader PRIVATE dl)
endif ()

target_include_directories(roblox_modloader PRIVATE
        "${ROBLOX_MODLOADER_SOURCE_DIR}"
        "${ROBLOX_MODLOADER_INCLUDE_DIR}"
)

set_target_properties(roblox_modloader PROPERTIES
        CXX_STANDARD 23
        CXX_STANDARD_REQUIRED ON
        CXX_EXTENSIONS OFF
        FOLDER "ModLoader"
        # Disable treating warnings as errors to avoid MSVC C2220 for third-party headers
        COMPILE_WARNING_AS_ERROR OFF
        ENABLE_EXPORTS ON
        MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL"
)

option(
        RML_FORCE_INCLUDE_COMMON_WITHOUT_PCH
        "Force-include the common header instead of emitting an MSVC PCH (useful for case-sensitive WSL paths)"
        OFF
)
if (RML_FORCE_INCLUDE_COMMON_WITHOUT_PCH)
    target_compile_options(roblox_modloader PRIVATE
            "$<$<CXX_COMPILER_ID:MSVC>:/FI${ROBLOX_MODLOADER_INCLUDE_DIR}/RobloxModLoader/internal/common.hpp>"
    )
else ()
    target_precompile_headers(roblox_modloader PRIVATE
            "${ROBLOX_MODLOADER_INCLUDE_DIR}/RobloxModLoader/internal/common.hpp"
    )
endif ()

if (BUILD_TESTING)
	add_executable(instance_child_reorder_tests EXCLUDE_FROM_ALL
			"${CMAKE_CURRENT_SOURCE_DIR}/tests/instance_child_reorder_tests.cpp"
	)
	target_include_directories(instance_child_reorder_tests PRIVATE
			"${ROBLOX_MODLOADER_SOURCE_DIR}"
	)
	target_compile_features(instance_child_reorder_tests PRIVATE cxx_std_23)
	set_target_properties(instance_child_reorder_tests PROPERTIES
			FOLDER "Tests"
			MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL"
	)
	add_test(NAME instance_child_reorder COMMAND instance_child_reorder_tests)

	add_executable(hierarchy_capture_fast_path_tests EXCLUDE_FROM_ALL
			"${CMAKE_CURRENT_SOURCE_DIR}/tests/hierarchy_capture_fast_path_tests.cpp"
	)
	target_include_directories(hierarchy_capture_fast_path_tests PRIVATE
			"${ROBLOX_MODLOADER_SOURCE_DIR}"
	)
	target_compile_features(hierarchy_capture_fast_path_tests PRIVATE cxx_std_23)
	set_target_properties(hierarchy_capture_fast_path_tests PROPERTIES
			FOLDER "Tests"
			MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL"
	)
	add_test(NAME hierarchy_capture_fast_path COMMAND hierarchy_capture_fast_path_tests)

    add_executable(serialized_property_event_gate_tests EXCLUDE_FROM_ALL
            "${CMAKE_CURRENT_SOURCE_DIR}/tests/serialized_property_event_gate_tests.cpp"
    )
    target_include_directories(serialized_property_event_gate_tests PRIVATE
            "${ROBLOX_MODLOADER_SOURCE_DIR}"
            "${ROBLOX_MODLOADER_INCLUDE_DIR}"
    )
    target_compile_features(serialized_property_event_gate_tests PRIVATE cxx_std_23)
    set_target_properties(serialized_property_event_gate_tests PROPERTIES
            FOLDER "Tests"
            MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL"
    )
    add_test(NAME serialized_property_event_gate COMMAND serialized_property_event_gate_tests)

    add_executable(foreground_activation_policy_tests EXCLUDE_FROM_ALL
            "${CMAKE_CURRENT_SOURCE_DIR}/tests/foreground_activation_policy_tests.cpp"
    )
    target_include_directories(foreground_activation_policy_tests PRIVATE
            "${ROBLOX_MODLOADER_SOURCE_DIR}"
    )
    target_compile_features(foreground_activation_policy_tests PRIVATE cxx_std_23)
    set_target_properties(foreground_activation_policy_tests PROPERTIES
            FOLDER "Tests"
            MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL"
    )
    add_test(NAME foreground_activation_policy COMMAND foreground_activation_policy_tests)
    add_executable(datamodel_rtti_capability_tests EXCLUDE_FROM_ALL
            "${CMAKE_CURRENT_SOURCE_DIR}/tests/reflection_runtime_resolver_tests.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/reflection/runtime_layout_resolver.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/reflection/capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/datamodel_layout_resolver.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/datamodel_capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/data_model.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/job_capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/instance_capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/memory/symbol_resolver.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/utils/name.cpp"
    )
    target_include_directories(datamodel_rtti_capability_tests PRIVATE
            "${ROBLOX_MODLOADER_SOURCE_DIR}"
            "${ROBLOX_MODLOADER_INCLUDE_DIR}"
    )
    setup_core_dependencies(datamodel_rtti_capability_tests PRIVATE)
    setup_luau_dependencies(datamodel_rtti_capability_tests PRIVATE)
    target_link_libraries(datamodel_rtti_capability_tests PRIVATE Zydis rml_thirdparty)
    target_compile_definitions(datamodel_rtti_capability_tests PRIVATE ROBLOX_MODLOADER_STATIC_DEFINE)
    target_compile_features(datamodel_rtti_capability_tests PRIVATE cxx_std_23)
    set_target_properties(datamodel_rtti_capability_tests PROPERTIES
            FOLDER "Tests"
            MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL"
    )
    add_test(NAME reflection_runtime_resolver COMMAND datamodel_rtti_capability_tests)
    add_executable(offline_abi_resolver_tests EXCLUDE_FROM_ALL
            "${CMAKE_CURRENT_SOURCE_DIR}/tests/offline_abi_resolver_tests.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/reflection/runtime_layout_resolver.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/reflection/capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/datamodel_layout_resolver.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/datamodel_capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/data_model.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/job_capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/roblox/instance_capabilities.cpp"
            "${ROBLOX_MODLOADER_SOURCE_DIR}/memory/symbol_resolver.cpp"
    )
    target_include_directories(offline_abi_resolver_tests PRIVATE
            "${ROBLOX_MODLOADER_SOURCE_DIR}"
            "${ROBLOX_MODLOADER_INCLUDE_DIR}"
    )
    setup_core_dependencies(offline_abi_resolver_tests PRIVATE)
    setup_luau_dependencies(offline_abi_resolver_tests PRIVATE)
    target_link_libraries(offline_abi_resolver_tests PRIVATE Zydis rml_thirdparty)
    target_compile_definitions(offline_abi_resolver_tests PRIVATE ROBLOX_MODLOADER_STATIC_DEFINE)
    target_compile_features(offline_abi_resolver_tests PRIVATE cxx_std_23)
    set_target_properties(offline_abi_resolver_tests PROPERTIES
            FOLDER "Tests"
            MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL"
    )
    add_test(NAME offline_abi_resolver COMMAND offline_abi_resolver_tests)
endif ()

include(GenerateExportHeader)
generate_export_header(roblox_modloader
        EXPORT_FILE_NAME "${ROBLOX_MODLOADER_INCLUDE_DIR}/RobloxModLoader/rml_export.hpp"
        EXPORT_MACRO_NAME "RML_EXPORT"
        NO_EXPORT_MACRO_NAME "RML_NO_EXPORT"
        DEPRECATED_MACRO_NAME "RML_DEPRECATED"
)

add_library(RobloxModLoader INTERFACE)
add_library(RobloxModLoader::RobloxModLoader ALIAS RobloxModLoader)

target_include_directories(RobloxModLoader INTERFACE
        "$<BUILD_INTERFACE:${ROBLOX_MODLOADER_INCLUDE_DIR}>"
        "$<INSTALL_INTERFACE:include>"
        "$<BUILD_INTERFACE:${ROBLOX_MODLOADER_ROOT_DIR}/libs/third_party/g3d/include>"
        "$<BUILD_INTERFACE:${ROBLOX_MODLOADER_ROOT_DIR}/libs/third_party/bullet/include>"
        "$<BUILD_INTERFACE:${ROBLOX_MODLOADER_ROOT_DIR}/libs/third_party/rbxg3d/include>"
        "$<BUILD_INTERFACE:${spdlog_SOURCE_DIR}/include>"
        "$<BUILD_INTERFACE:${tomlplusplus_SOURCE_DIR}/include>"
        "$<BUILD_INTERFACE:${nlohmann_json_SOURCE_DIR}/include>"
        "$<BUILD_INTERFACE:${minhook_SOURCE_DIR}/include>"
        "$<BUILD_INTERFACE:${LUAU_SOURCE_DIR}/Compiler/include>"
        "$<BUILD_INTERFACE:${LUAU_SOURCE_DIR}/Ast/include>"
        "$<BUILD_INTERFACE:${LUAU_SOURCE_DIR}/VM/include>"
        "$<BUILD_INTERFACE:${LUAU_SOURCE_DIR}/Common/include>"
        "$<BUILD_INTERFACE:${polyhook2_SOURCE_DIR}>"
)

target_link_libraries(RobloxModLoader INTERFACE
        spdlog::spdlog
        nlohmann_json::nlohmann_json
        tomlplusplus::tomlplusplus
        minhook
        PolyHook_2
        Luau.Compiler
        Luau.Ast
        Luau.VM
        Luau.VM.Internals
        Luau.CodeGen
        $<$<TARGET_EXISTS:roblox_modloader>:roblox_modloader>
)

target_compile_features(RobloxModLoader INTERFACE cxx_std_23)

target_compile_definitions(RobloxModLoader INTERFACE
        _CRT_SECURE_NO_WARNINGS
        NOMINMAX
        WIN32_LEAN_AND_MEAN
        IS_RML=1
        TOML_EXCEPTIONS=0
        $<$<CONFIG:Debug>:DEBUG>
)

setup_dotnet_hosting(RobloxModLoader INTERFACE)

set(RML_LUA_LIBS_DIR "${CMAKE_CURRENT_SOURCE_DIR}/resources/luau")

if (ROBLOX_MODLOADER_INSTALL)
    include(GNUInstallDirs)
    install(DIRECTORY "${RML_LUA_LIBS_DIR}/"
            DESTINATION ${CMAKE_INSTALL_DATADIR}/RobloxModLoader/libraries
            FILES_MATCHING PATTERN "*.luau"
    )
endif ()

if (WIN32 AND TARGET roblox_modloader AND DEFINED ENV{ROBLOX_STUDIO_PATH} AND NOT "$ENV{ROBLOX_STUDIO_PATH}" STREQUAL "")
    add_custom_command(TARGET roblox_modloader POST_BUILD
            COMMAND ${CMAKE_COMMAND} -E make_directory "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/libraries"
            COMMAND ${CMAKE_COMMAND} -E copy_directory
            "${RML_LUA_LIBS_DIR}"
            "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/libraries"
            COMMENT "Copying runtime Luau libraries to RobloxModLoader/libraries"
            VERBATIM
    )

    if (RML_DOTNET_RUNTIME_ROOT AND EXISTS "${RML_DOTNET_RUNTIME_ROOT}")
        add_custom_command(TARGET roblox_modloader POST_BUILD
                COMMAND ${CMAKE_COMMAND} -E make_directory "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/runtime"
                COMMAND ${CMAKE_COMMAND} -E copy_directory
                "${RML_DOTNET_RUNTIME_ROOT}"
                "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/runtime/dotnet"
                COMMENT "Copying bundled .NET runtime to RobloxModLoader/runtime/dotnet"
        )
    endif ()

    if (RML_DOTNET_GLOBAL_RUNTIME_CONFIG AND EXISTS "${RML_DOTNET_GLOBAL_RUNTIME_CONFIG}")
        add_custom_command(TARGET roblox_modloader POST_BUILD
                COMMAND ${CMAKE_COMMAND} -E make_directory "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/runtime"
                COMMAND ${CMAKE_COMMAND} -E copy_if_different
                "${RML_DOTNET_GLOBAL_RUNTIME_CONFIG}"
                "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/runtime/RobloxModLoader.runtimeconfig.json"
                COMMENT "Copying global .NET runtimeconfig to RobloxModLoader/runtime"
        )
    endif ()

    if (RML_DOTNET_NETHOST_RUNTIME_BINARY AND EXISTS "${RML_DOTNET_NETHOST_RUNTIME_BINARY}")
        get_filename_component(RML_NETHOST_RUNTIME_NAME "${RML_DOTNET_NETHOST_RUNTIME_BINARY}" NAME)
        add_custom_command(TARGET roblox_modloader POST_BUILD
                COMMAND ${CMAKE_COMMAND} -E make_directory "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/runtime"
                COMMAND ${CMAKE_COMMAND} -E copy_if_different
                "${RML_DOTNET_NETHOST_RUNTIME_BINARY}"
                "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader/runtime/${RML_NETHOST_RUNTIME_NAME}"
                COMMENT "Copying nethost runtime binary to RobloxModLoader/runtime"
        )
    endif ()

    add_custom_command(TARGET roblox_modloader POST_BUILD
            COMMAND ${CMAKE_COMMAND} -E make_directory "$ENV{ROBLOX_STUDIO_PATH}/RobloxModLoader"
            COMMAND ${CMAKE_COMMAND} -E copy_if_different
            $<TARGET_FILE:roblox_modloader>
            "$ENV{ROBLOX_STUDIO_PATH}/$<TARGET_FILE_NAME:roblox_modloader>"
            COMMENT "Copying built roblox_modloader DLL to RobloxModLoader root"
            VERBATIM
    )
endif ()
