/* One segment, file offsets equal to addresses, as the arm64 kernel's vdso.lds lays it out: the
 * image is mapped as it is. The data page is last. */
SECTIONS
{
    . = SIZEOF_HEADERS;
    .hash : { *(.hash) } :text
    .gnu.hash : { *(.gnu.hash) } :text
    .dynsym : { *(.dynsym) } :text
    .dynstr : { *(.dynstr) } :text
    .dynamic : { *(.dynamic) } :text :dynamic
    .text : { *(.text*) } :text
    . = ALIGN(4096);
    .vvar : { *(.vvar) } :text
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
PHDRS
{
    text PT_LOAD FLAGS(5) FILEHDR PHDRS;
    dynamic PT_DYNAMIC FLAGS(4);
}
