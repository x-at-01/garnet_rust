# WeDB vs WeDb-BfTree vs RocksDB vs Fjall Deep Benchmark Report

## 1. Benchmark Environment & Configuration

- **Write Operations: Sequential 150,000 ops / Random 150,000 ops**
- **Read Operations: 150,000 ops**
- **Payload Value Size: 128 bytes**
- **Aligned Memory Budget: 256 MB (Strictly 1:1 byte-level alignment across all engines)**
- **Benchmark Mode: Pure storage engine end-to-end fair evaluation (no external sidecar cache)**

## 2. Core Architecture Matrix

| Dimension | WeDB (Tsavorite Architecture) | WeDb-BfTree (In-House Garnet B-Tree) | RocksDB (Production LSM-Tree) | Fjall (Modern Rust LSM-Tree) |
| :--- | :--- | :--- | :--- | :--- |
| **Primary Index** | 64B Lock-free HashIndex + HybridLog | Block-level Bε-tree / Paged B-tree | Multi-level SSTable + Memtable SkipList | Memtable + WAL + Multi-tier SSTable |
| **Write Path** | In-Place Update (Mutable Region) / Tail Append | Paged In-Place Update + Leaf Merge | Memtable Append + WAL Disk Sync | Memtable Append + WAL Sequential Sync |
| **Concurrency** | Zero system locks, atomic CAS + LightEpoch | Lock striping + fine-grained concurrency | Optimistic MVCC + mutex batching | Safe multi-thread sharing + snapshot reads |
| **Underlying I/O** | Native compio (io_uring / IOCP) | Sync / Paged block file I/O | Standard POSIX sync file I/O | Standard synchronous file I/O + page cache |
| **Target Use Cases** | Ultra-high throughput point read/write, in-place updates | Range slices, ordered key scans, tiered storage | High write volume, compaction, high compression ratio | Pure Rust lightweight embedded persistent store |

## 3. Benchmark Results by Workload Category

### Table 1: Single-Point Write Throughput

| Benchmark Workload | WeDB (Throughput) | WeDb-BfTree (Throughput) | RocksDB (Throughput) | Fjall (Throughput) | WeDB vs RocksDB Speedup |
| :--- | :---: | :---: | :---: | :---: | :---: |
| 1. Sequential Insert | 0.797 GB/s | 0.153 GB/s | 0.039 GB/s | 0.076 GB/s | **20.32x** |
| 2. Random Insert | 0.716 GB/s | 0.189 GB/s | 0.034 GB/s | 0.064 GB/s | **20.93x** |
| 3. Hot In-Place Update | 1.08 GB/s | 0.668 GB/s | 0.037 GB/s | 0.056 GB/s | **29.58x** |

### Table 2: Single-Point Read & Delete Throughput

| Benchmark Workload | WeDB (Throughput) | WeDb-BfTree (Throughput) | RocksDB (Throughput) | Fjall (Throughput) | WeDB vs RocksDB Speedup |
| :--- | :---: | :---: | :---: | :---: | :---: |
| 4. Point Read Hit | 0.394 GB/s | 0.337 GB/s | 0.095 GB/s | 0.121 GB/s | **4.13x** |
| 5. Point Read Miss | 0.173 GB/s | 0.023 GB/s | 0.022 GB/s | 0.076 GB/s | **7.90x** |
| 6. Point Delete | 0.054 GB/s | 0.070 GB/s | 0.0038 GB/s | 0.0090 GB/s | **14.01x** |

### Table 3: Concurrency & Range Query Throughput

| Benchmark Workload | WeDB (Throughput) | WeDb-BfTree (Throughput) | RocksDB (Throughput) | Fjall (Throughput) | WeDB vs RocksDB Speedup |
| :--- | :---: | :---: | :---: | :---: | :---: |
| 7. High Concurrency (4 Threads 80%R/20%W) | 1.30 GB/s | 0.203 GB/s | 0.088 GB/s | 0.084 GB/s | **14.82x** |
| 8. Dynamic Range Query | 0.060 GB/s | 2.57 GB/s | 0.689 GB/s | 1.03 GB/s | 0.09x |

### Table 4: Single-Point Write Latency

| Benchmark Workload | WeDB (P50 Latency (μs)) | WeDb-BfTree (P50 Latency (μs)) | RocksDB (P50 Latency (μs)) | Fjall (P50 Latency (μs)) |
| :--- | :---: | :---: | :---: | :---: |
| 1. Sequential Insert | 0.12 / 0.25 μs | 0.17 / 33.21 μs | 2.50 / 19.21 μs | 1.04 / 8.42 μs |
| 2. Random Insert | 0.12 / 0.25 μs | 0.25 / 6.71 μs | 3.21 / 20.08 μs | 1.29 / 8.58 μs |
| 3. Hot In-Place Update | 0.04 / 0.25 μs | 0.17 / 0.29 μs | 2.75 / 19.50 μs | 1.12 / 8.33 μs |

### Table 5: Single-Point Read & Delete Latency

| Benchmark Workload | WeDB (P50 Latency (μs)) | WeDb-BfTree (P50 Latency (μs)) | RocksDB (P50 Latency (μs)) | Fjall (P50 Latency (μs)) |
| :--- | :---: | :---: | :---: | :---: |
| 4. Point Read Hit | 0.29 / 0.75 μs | 0.33 / 0.79 μs | 1.33 / 3.04 μs | 1.04 / 2.88 μs |
| 5. Point Read Miss | 0.08 / 0.21 μs | 0.75 / 1.00 μs | 0.79 / 1.83 μs | 0.21 / 0.62 μs |
| 6. Point Delete | 0.21 / 0.33 μs | 0.17 / 0.33 μs | 2.92 / 9.04 μs | 1.17 / 6.29 μs |

### Table 6: Concurrency & Range Query Latency

| Benchmark Workload | WeDB (P50 Latency (μs)) | WeDb-BfTree (P50 Latency (μs)) | RocksDB (P50 Latency (μs)) | Fjall (P50 Latency (μs)) |
| :--- | :---: | :---: | :---: | :---: |
| 7. High Concurrency (4 Threads 80%R/20%W) | 0.10 / 0.00 μs | 0.63 / 0.00 μs | 1.45 / 0.00 μs | 1.53 / 0.00 μs |
| 8. Dynamic Range Query | 22.75 / 72.46 μs | 3.29 / 13.29 μs | 13.58 / 45.54 μs | 9.25 / 35.71 μs |

### Table 7: Overall Geometric Mean Throughput & Disk Footprint

| Storage Engine | Architecture | Weighted Geomean Throughput | Geomean QPS | Relative vs RocksDB | Disk Footprint (MB) | Summary |
| :--- | :--- | :---: | :---: | :---: | :---: | :--- |
| **WeDB** | Tsavorite HybridLog + HashIndex | **0.434 GB/s** | 2,838,506 ops/s | **6.08x** | 55.94 MB | Blazing point access, in-place update, lock-free concurrency |
| **WeDb-BfTree** | In-House Garnet Ordered B-Tree | 0.323 GB/s | 1,574,868 ops/s | 4.51x | 116.31 MB | Native block-level range scan (#1 overall), high cache efficiency |
| **RocksDB** | Facebook Industrial LSM-Tree | 0.071 GB/s | 348,917 ops/s | 1.00x | 4.54 MB | Versatile cold storage, high compression, level compactions |
| **Fjall** | Pure Rust LSM-Tree | 0.106 GB/s | 518,294 ops/s | 1.49x | 108.47 MB | Modern pure Rust codebase, ordered slice iterators |

## 4. Technical Insights & Architectural Analysis

1. In-Place Updates: WeDB exhibits dramatic throughput advantages because HybridLog defines an in-memory Mutable Region. Existing keys in this region are updated in-place by atomic pointer or memory updates, avoiding new allocations, WAL overhead, or compactions.

2. Point Read Hit: WeDB leverages 64-byte cacheline-aligned hash buckets with 14-bit tags for direct O(1) addressing, without skip-list or multi-level tree traversal overhead; WeDb-BfTree locates nodes via paged memory cache; RocksDB and Fjall require Memtable lookups or BloomFilter + Block Cache traversal.

3. Concurrency Scalability: WeDB's LightEpoch provides lock-free thread coordination, where each thread operates via independent sessions, enabling near-linear multi-threaded scaling; WeDb-BfTree utilizes striped concurrency; RocksDB is constrained by write mutex batching.

4. Range Queries: WeDb-BfTree is our in-house implementation based on Microsoft Garnet's bftree-garnet. Operating with block paging and leaf cache, its range query throughput ranks #1 among all engines; RocksDB and Fjall support total lexicographical scans; WeDB decouples range ordering into object-layer skip lists (SortedSet).

