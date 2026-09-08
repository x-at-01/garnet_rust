# wedb_blocking

High-performance Redis / Garnet blocking command scheduling broker based on compio async ecosystem and crossfire channels.

1:1 aligned with Microsoft Garnet `CollectionItemBroker`, supporting blocking primitives including `BLPOP`, `BRPOP`, `BLMOVE`, `BLMPOP`, `BRPOPLPUSH`, `BZPOPMIN`, `BZPOPMAX`, and `BZMPOP`.