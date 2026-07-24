if (NOT DEFINED CPM_SOURCE_CACHE AND NOT DEFINED ENV{CPM_SOURCE_CACHE})
    set(CPM_SOURCE_CACHE "${CMAKE_BINARY_DIR}/_cpm_cache" CACHE PATH "Directory CPM caches fetched package sources in")
endif ()

set(CPM_DOWNLOAD_VERSION 0.40.2)
set(CPM_DOWNLOAD_SHA256 c8cdc32c03816538ce22781ed72964dc864b2a34a310d3b7104812a5ca2d835d)

if (CPM_SOURCE_CACHE)
    set(CPM_DOWNLOAD_LOCATION "${CPM_SOURCE_CACHE}/cpm/CPM_${CPM_DOWNLOAD_VERSION}.cmake")
elseif (DEFINED ENV{CPM_SOURCE_CACHE})
    set(CPM_DOWNLOAD_LOCATION "$ENV{CPM_SOURCE_CACHE}/cpm/CPM_${CPM_DOWNLOAD_VERSION}.cmake")
else ()
    set(CPM_DOWNLOAD_LOCATION "${CMAKE_BINARY_DIR}/cmake/CPM_${CPM_DOWNLOAD_VERSION}.cmake")
endif ()

get_filename_component(CPM_DOWNLOAD_LOCATION ${CPM_DOWNLOAD_LOCATION} ABSOLUTE)

function(download_cpm)
    message(STATUS "Downloading CPM.cmake to ${CPM_DOWNLOAD_LOCATION}")
    file(DOWNLOAD
            https://github.com/cpm-cmake/CPM.cmake/releases/download/v${CPM_DOWNLOAD_VERSION}/CPM.cmake
            ${CPM_DOWNLOAD_LOCATION}
            EXPECTED_HASH SHA256=${CPM_DOWNLOAD_SHA256}
            TLS_VERIFY ON
            STATUS download_status
    )
    list(GET download_status 0 download_code)
    list(GET download_status 1 download_message)
    if (NOT download_code EQUAL 0)
        file(REMOVE ${CPM_DOWNLOAD_LOCATION})
        message(FATAL_ERROR "CPM.cmake download failed: ${download_message}")
    endif ()
endfunction()

if (EXISTS ${CPM_DOWNLOAD_LOCATION})
    file(SHA256 ${CPM_DOWNLOAD_LOCATION} existing_cpm_sha256)
    if (NOT existing_cpm_sha256 STREQUAL CPM_DOWNLOAD_SHA256)
        message(STATUS "Replacing CPM.cmake with the pinned verified release")
        file(REMOVE ${CPM_DOWNLOAD_LOCATION})
    endif ()
    unset(existing_cpm_sha256)
endif ()

if (NOT (EXISTS ${CPM_DOWNLOAD_LOCATION}))
    download_cpm()
endif ()

include(${CPM_DOWNLOAD_LOCATION})
