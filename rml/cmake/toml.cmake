include(FetchContent)

message("TOML")
FetchContent_Declare(
        tomlplusplus
        GIT_REPOSITORY https://github.com/marzer/tomlplusplus
        GIT_TAG 30172438cee64926dc41fdd9c11fb3ba5b2ba9de
        GIT_PROGRESS TRUE
)

add_compile_definitions(TOML_EXCEPTIONS=0)
FetchContent_MakeAvailable(tomlplusplus)
