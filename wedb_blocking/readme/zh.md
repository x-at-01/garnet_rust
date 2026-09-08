# wedb_blocking

基于 compio 异步生态与 crossfire 消息通道的高性能阻塞命令调度中继器。

1:1 对标 Microsoft Garnet `CollectionItemBroker`，负责支撑 `BLPOP`, `BRPOP`, `BLMOVE`, `BLMPOP`, `BRPOPLPUSH`, `BZPOPMIN`, `BZPOPMAX`, `BZMPOP` 等阻塞命令调度。