# WeDB Benchmark


## 1. Environment & Configuration

- Write Operations: Sequential 500,000 ops / Random 500,000 ops

- Read Operations: 500,000 ops

- Dataset Size: ~1002.7 MB (1,035,986 records)

- Total Operations: 4,110,000 ops

- Payload Size: 1000 bytes (Key: 9 ~ 16 bytes)

- Memory Budget: 256 MB

- Process Peak RSS: 1708.36 MB

- Storage engine benchmark, zero external cache


## 2. Architecture Comparison

- **WeDB**: HybridLog circular buffer + 64B lock-free hash index. Features: divided mutable and read-only memory regions, in-place updates for hot data, lock-free tail append for cold data; LightEpoch concurrency coordination; native async I/O; suited for point operations and microsecond latency.

- **WeDb-BfTree**: Block-level ordered B-tree with disk paging (benchmarked against Garnet BfTree). Features: fixed block paging, striped concurrency locks; leaf page cache and paged in-place updates; cached block file I/O; suited for range scans and ordered slices.

- **RocksDB**: Multi-level SSTables + Memtable skip list. Features: writes append to Memtable and sequential WAL flush; background tiered compaction reclaims dead keys with block compression; point reads query BloomFilter and Block Cache; suited for persistence and high compression.

- **Fjall**: LSM architecture + WAL. Features: standard library sync file I/O, thread-safe sharing and snapshot reads; tiered SSTable compaction for ordered iteration; suited for ordered embedded storage.


## 3. Benchmark

### 1. Sequential Write

| Metric              |    WeDB    | WeDb-BfTree |  RocksDB   |   **Fjall**    |
| :------------------ | :--------: | :---------: | :--------: | :------------: |
| Throughput (GB/s)   | 0.029 GB/s | 0.017 GB/s  | 0.268 GB/s | **0.347 GB/s** |
| Rate (ops/s)        |   30,282   |   18,471    |  283,322   |  **367,632**   |
| P50 Latency (μs)    |  **0.25**  |  **0.25**   |    2.42    |      1.21      |
| P99 Latency (μs)    |  **5.33**  |    95.58    |   11.79    |      6.79      |
| Relative vs RocksDB |   0.11x    |    0.07x    |   1.00x    |   **1.30x**    |

### 2. Random Write

| Metric              |   **WeDB**    | WeDb-BfTree |  RocksDB   |   Fjall    |
| :------------------ | :-----------: | :---------: | :--------: | :--------: |
| Throughput (GB/s)   | **1.02 GB/s** | 0.512 GB/s  | 0.214 GB/s | 0.271 GB/s |
| Rate (ops/s)        | **1,077,409** |   540,987   |  226,444   |  285,972   |
| P50 Latency (μs)    |   **0.21**    |    0.63     |    3.50    |    1.67    |
| P99 Latency (μs)    |   **1.21**    |    14.75    |   11.42    |   12.00    |
| Relative vs RocksDB |   **4.76x**   |    2.39x    |   1.00x    |   1.26x    |

### 3. Sequential Read

| Metric              |    WeDB    | WeDb-BfTree |  RocksDB   |   **Fjall**    |
| :------------------ | :--------: | :---------: | :--------: | :------------: |
| Throughput (GB/s)   | 0.367 GB/s | 0.608 GB/s  | 0.591 GB/s | **0.813 GB/s** |
| Rate (ops/s)        |  388,831   |   643,656   |  625,526   |  **861,060**   |
| P50 Latency (μs)    |  **0.44**  |    0.88     |    1.00    |      0.75      |
| P99 Latency (μs)    |   21.53    |    13.63    |    6.46    |    **5.17**    |
| Relative vs RocksDB |   0.62x    |    1.03x    |   1.00x    |   **1.38x**    |

### 4. Random Read

| Metric              |    WeDB    | **WeDb-BfTree** |  RocksDB   |   Fjall    |
| :------------------ | :--------: | :-------------: | :--------: | :--------: |
| Throughput (GB/s)   | 0.275 GB/s | **0.352 GB/s**  | 0.200 GB/s | 0.314 GB/s |
| Rate (ops/s)        |  290,424   |   **372,142**   |  211,476   |  331,382   |
| P50 Latency (μs)    |    3.13    |    **2.33**     |    4.21    |    2.92    |
| P99 Latency (μs)    |  **6.43**  |      13.54      |   12.21    |    7.96    |
| Relative vs RocksDB |   1.37x    |    **1.76x**    |   1.00x    |   1.57x    |

### 5. Update Heavy (50% Read / 50% Write)

| Metric              |    WeDB    | **WeDb-BfTree** |  RocksDB   |   Fjall    |
| :------------------ | :--------: | :-------------: | :--------: | :--------: |
| Throughput (GB/s)   | 0.626 GB/s | **0.784 GB/s**  | 0.247 GB/s | 0.320 GB/s |
| Rate (ops/s)        |  662,965   |   **830,661**   |  261,820   |  338,610   |
| P50 Latency (μs)    |  **0.38**  |      0.46       |    3.13    |    1.67    |
| P99 Latency (μs)    |   12.08    |    **7.75**     |   14.04    |    9.00    |
| Relative vs RocksDB |   2.53x    |    **3.17x**    |   1.00x    |   1.29x    |

### 6. Read Mostly (95% Read / 5% Write)

| Metric              |    WeDB    | **WeDb-BfTree** |  RocksDB   |   Fjall    |
| :------------------ | :--------: | :-------------: | :--------: | :--------: |
| Throughput (GB/s)   | 0.470 GB/s | **0.957 GB/s**  | 0.418 GB/s | 0.660 GB/s |
| Rate (ops/s)        |  497,891   |  **1,013,893**  |  442,297   |  699,070   |
| P50 Latency (μs)    |  **0.29**  |      0.42       |    1.04    |    0.58    |
| P99 Latency (μs)    |   16.54    |      6.50       |   10.33    |  **5.79**  |
| Relative vs RocksDB |   1.13x    |    **2.29x**    |   1.00x    |   1.58x    |

### 7. Read Only (100% Read)

| Metric              |    WeDB    | **WeDb-BfTree** |  RocksDB   |   Fjall    |
| :------------------ | :--------: | :-------------: | :--------: | :--------: |
| Throughput (GB/s)   | 0.388 GB/s |  **1.10 GB/s**  | 0.502 GB/s | 0.656 GB/s |
| Rate (ops/s)        |  410,667   |  **1,166,586**  |  531,533   |  694,246   |
| P50 Latency (μs)    |  **0.29**  |      0.38       |    0.92    |    0.58    |
| P99 Latency (μs)    |   19.13    |    **4.88**     |    8.00    |    7.79    |
| Relative vs RocksDB |   0.77x    |    **2.19x**    |   1.00x    |   1.31x    |

### 8. Read Latest (95% Read Latest / 5% Append)

| Metric              |   **WeDB**    | WeDb-BfTree |  RocksDB  |   Fjall   |
| :------------------ | :-----------: | :---------: | :-------: | :-------: |
| Throughput (GB/s)   | **6.70 GB/s** |  3.75 GB/s  | 1.25 GB/s | 2.95 GB/s |
| Rate (ops/s)        | **7,091,788** |  3,968,813  | 1,328,308 | 3,118,699 |
| P50 Latency (μs)    |   **0.04**    |    0.13     |   0.54    |   0.21    |
| P99 Latency (μs)    |   **0.54**    |    0.67     |   4.79    |   2.38    |
| Relative vs RocksDB |   **5.34x**   |    2.99x    |   1.00x   |   2.35x   |

### 9. Range Query

| Metric              |    **WeDB**    | WeDb-BfTree |  RocksDB  |   Fjall   |
| :------------------ | :------------: | :---------: | :-------: | :-------: |
| Throughput (GB/s)   | **15.82 GB/s** | 11.78 GB/s  | 6.81 GB/s | 7.97 GB/s |
| Rate (ops/s)        |  **211,116**   |   157,311   |  90,888   |  106,378  |
| P50 Latency (μs)    |    **4.54**    |    5.54     |   10.63   |   8.46    |
| P99 Latency (μs)    |   **12.79**    |    24.04    |   23.63   |   32.67   |
| Relative vs RocksDB |   **2.32x**    |    1.73x    |   1.00x   |   1.17x   |

### 10. Concurrency (4 Threads 80% Read / 20% Write)

| Metric              |   **WeDB**    | WeDb-BfTree |  RocksDB   |   Fjall    |
| :------------------ | :-----------: | :---------: | :--------: | :--------: |
| Throughput (GB/s)   | **4.81 GB/s** |  1.11 GB/s  | 0.517 GB/s | 0.405 GB/s |
| Rate (ops/s)        | **5,120,349** |  1,182,346  |  550,399   |  431,053   |
| P50 Latency (μs)    |   **0.33**    |    1.96     |    1.79    |    0.88    |
| P99 Latency (μs)    |   **1.33**    |    31.96    |   59.54    |   87.54    |
| Relative vs RocksDB |   **9.30x**   |    2.15x    |   1.00x    |   0.78x    |

### Overall Throughput

| Engine      | Geomean Throughput |    Geomean QPS    | Relative vs RocksDB |
| :---------- | :----------------: | :---------------: | :-----------------: |
| **WeDB**    |   **0.797 GB/s**   | **594,520 ops/s** |      **1.63x**      |
| WeDb-BfTree |     0.767 GB/s     |   572,650 ops/s   |        1.57x        |
| RocksDB     |     0.490 GB/s     |   365,394 ops/s   |        1.00x        |
| Fjall       |     0.684 GB/s     |   510,305 ops/s   |        1.40x        |


### Disk Footprint

- Raw Size: 1002.73 MB

| Engine      | Disk Footprint | Space Amplification | Storage & Write Patterns                           |
| :---------- | :------------: | :-----------------: | :------------------------------------------------- |
| WeDB        |   1100.50 MB   |        1.10x        | Segmented log preallocation, sequential append     |
| WeDb-BfTree |   3456.88 MB   |        3.45x        | Fixed-size block paging with free page reclamation |
| **RocksDB** |   643.94 MB    |      **0.64x**      | Multi-level SSTables + block compression           |
| Fjall       |   1248.18 MB   |        1.24x        | Tiered compaction & block persistence              |


### Memory Usage

- Memory Budget: 256.00 MB

| Engine      | Resident Memory | Budget Utilization | Memory Management                            |
| :---------- | :-------------: | :----------------: | :------------------------------------------- |
| WeDB        |    284.4 MB     |       111.1%       | Bounded ring buffer & lock-free hash buckets |
| WeDb-BfTree |    256.0 MB     |       100.0%       | LRU block page cache                         |
| RocksDB     |    256.0 MB     |       100.0%       | Block Cache (75%) + Memtable (25%)           |
| Fjall       |    256.0 MB     |       100.0%       | Block Cache & Memtable soft limits           |


## 4. Technical Analysis

1. In-Place Updates: WeDB performs in-place updates in the HybridLog mutable region, without WAL append or compactions. LSM-Tree engines (RocksDB & Fjall) append updates to Memtable, triggering background compactions and write amplification.


2. Point Reads: WeDB locates records via a 64B hash index in O(1) time complexity; WeDb-BfTree traverses nodes via paged memory cache; RocksDB and Fjall query Memtable, BloomFilter, and Block Cache.


3. Concurrency: WeDB uses LightEpoch coordination where threads access independent sessions with atomic append; WeDb-BfTree uses striped locks; RocksDB is bounded by write mutex batching.


4. Range Queries: all four engines use a unified raw-KV ordered scan methodology (WeDB drives its built-in BfTree raw KV scan directly, bypassing the ZSet upper layer); WeDb-BfTree benchmarks against Garnet BfTree block paging; RocksDB and Fjall support full lexicographical range scans.


5. Space Amplification: LSM-Tree engines achieve space amplification below 1.0x via background tiered compaction and block compression; WeDB uses segmented log append with space amplification around 1.08x.


