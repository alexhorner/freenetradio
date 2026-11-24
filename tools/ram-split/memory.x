/* CH32V307VCT6 memory map for this board.
 *
 * The CH32V307 splits 320K of on-chip SRAM between zero-wait flash and RAM
 * via the SRAM_CODE_MODE option bytes. This chip is configured for
 * 288K flash + 32K RAM (as reported by `wlink` / the ESIG). If you change
 * the option bytes, update FLASH/RAM lengths below to match:
 *   192K/128K, 224K/96K, 256K/64K, 288K/32K
 */
MEMORY
{
    FLASH (rx)  : ORIGIN = 0x00000000, LENGTH = 288K /* boot alias of 0x08000000 */
    RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 32K
}

REGION_ALIAS("REGION_TEXT", FLASH);
REGION_ALIAS("REGION_RODATA", FLASH);
REGION_ALIAS("REGION_DATA", RAM);
REGION_ALIAS("REGION_BSS", RAM);
REGION_ALIAS("REGION_HEAP", RAM);
REGION_ALIAS("REGION_STACK", RAM);
