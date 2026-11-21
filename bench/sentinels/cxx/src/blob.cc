#include "sentinel-cxx/include/blob.h"

uint64_t checksum(rust::Slice<const uint8_t> bytes) {
  uint64_t sum = 1469598103934665603ull;
  for (auto byte : bytes) {
    sum = (sum ^ byte) * 1099511628211ull;
  }
  return sum;
}
