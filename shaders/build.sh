#!/bin/sh
# Compile the shaders to the SPIR-V that src/gpu.rs embeds. Needs glslc (shaderc).
set -eu
cd "$(dirname "$0")"
glslc -O fullscreen.vert -o fullscreen.vert.spv
glslc -O neon.frag -o neon.frag.spv
