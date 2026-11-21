#pragma once
#include "rust/cxx.h"
#include <cstdint>

uint64_t checksum(rust::Slice<const uint8_t> bytes);
