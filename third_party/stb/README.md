# stb_image

`stb_image.h` v2.30 from <https://github.com/nothings/stb>, public domain / MIT
(see the license block at the top of the header). It is compiled into
`userspace/engine/libwolfhtml.a` with JPEG, PNG and GIF enabled and used only
for in-guest page images (encoded bytes come from the guest net_server).
