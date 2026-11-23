# Stands in for Dawn when llama.cpp's WebGPU backend is built for an armless guest: dawn::webgpu_dawn
# is only include/webgpu/webgpu_cpp.h, which speaks armless-gpu's protocol to the host's GPU.
#   cmake ... -DGGML_WEBGPU=ON -DDawn_DIR=<this directory>
if(NOT TARGET dawn::webgpu_dawn)
    add_library(dawn::webgpu_dawn INTERFACE IMPORTED)
    set_target_properties(dawn::webgpu_dawn PROPERTIES
        INTERFACE_INCLUDE_DIRECTORIES "${CMAKE_CURRENT_LIST_DIR}/../include")
endif()
set(Dawn_FOUND TRUE)
