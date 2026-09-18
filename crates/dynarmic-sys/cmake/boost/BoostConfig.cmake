# CONFIG-mode package for the vendored, header-only Boost subset.
#
# dynarmic calls `find_package(Boost 1.57 REQUIRED)`. CMake's own FindBoost
# module has been deprecated since 3.30 and is slated for removal; this file
# means the vendored subset keeps being found the day that happens. Pointing
# `Boost_DIR` at this directory makes find_package take the CONFIG path and
# skip FindBoost entirely.
#
# Boost itself is BSL-1.0; see ../../vendor/boost/LICENSE_1_0.txt.

get_filename_component(_od_boost_include
                       "${CMAKE_CURRENT_LIST_DIR}/../../vendor/boost" ABSOLUTE)

if(NOT EXISTS "${_od_boost_include}/boost/version.hpp")
    set(Boost_FOUND FALSE)
    set(Boost_NOT_FOUND_MESSAGE
        "Omnidroid's vendored Boost subset is missing from ${_od_boost_include}")
    return()
endif()

set(Boost_INCLUDE_DIRS "${_od_boost_include}")
set(Boost_INCLUDE_DIR "${_od_boost_include}")
set(Boost_LIBRARIES "")
set(Boost_FOUND TRUE)

if(NOT TARGET Boost::headers)
    add_library(Boost::headers INTERFACE IMPORTED)
    set_target_properties(Boost::headers PROPERTIES
        INTERFACE_INCLUDE_DIRECTORIES "${_od_boost_include}")
endif()

# `Boost::boost` is the legacy spelling; dynarmic links against it by name.
if(NOT TARGET Boost::boost)
    add_library(Boost::boost INTERFACE IMPORTED)
    set_target_properties(Boost::boost PROPERTIES
        INTERFACE_INCLUDE_DIRECTORIES "${_od_boost_include}")
endif()
