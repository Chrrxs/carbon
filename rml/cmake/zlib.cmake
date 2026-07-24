include(FetchContent)

message("ZLIB")
FetchContent_Declare(
        zlib-cmake
        URL https://github.com/jimmy-park/zlib-cmake/archive/refs/tags/1.3.2.tar.gz
        URL_HASH SHA256=bba092e92862fda5d1497d4e39f8d41ce8479f40e850a8fc50aa36066922473a
        DOWNLOAD_EXTRACT_TIMESTAMP TRUE
)

FetchContent_MakeAvailable(zlib-cmake)
