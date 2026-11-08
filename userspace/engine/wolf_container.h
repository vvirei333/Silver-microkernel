// litehtml -> Wolfser surface bridge.
//
// litehtml only computes layout and calls back into this container for every
// glyph, fill and border. The container therefore owns no window and no font
// engine: it writes into the guest's RGBA ivshmem surface and asks Rust for
// the 5x7 glyph drawing (the same font the text renderer uses).

#ifndef WOLF_CONTAINER_H
#define WOLF_CONTAINER_H

#include "litehtml.h"

#include <cstdint>
#include <string>
#include <vector>

/// One drawing target. litehtml receives a pointer to this through `hdc`.
struct WolfCanvas
{
    uint32_t* pixels = nullptr;
    int       width = 0;  // surface width in pixels
    int       height = 0; // surface height in pixels
    int       stride = 0; // pixels per row
    bool      has_clip = false;
    int       clip_x = 0;
    int       clip_y = 0;
    int       clip_w = 0;
    int       clip_h = 0;
    std::vector<litehtml::position> clip_stack;
};

extern "C"
{
    /// Width of `text` in pixels at the given integer font scale.
    uint32_t wolf_text_width(const char* text, uint32_t len, uint32_t scale);

    /// Draw `text` into a 0xAABBGGRR surface. `clip_*` is in surface pixels.
    void wolf_draw_text(uint32_t* pixels, int32_t stride, int32_t width, int32_t height,
                        int32_t x, int32_t y, const char* text, uint32_t len, uint32_t scale,
                        uint32_t color, int32_t clip_x, int32_t clip_y, int32_t clip_w,
                        int32_t clip_h);

    /// Fetch `src` (resolved against `base`) through the guest net_server.
    /// Returns the encoded byte count, or -1. Called from litehtml during
    /// layout; it blocks the browser task while the net_server replies.
    int32_t wolf_fetch_image(const uint8_t* src, uint32_t src_len, const uint8_t* base,
                             uint32_t base_len, uint8_t* out, uint32_t cap);
}

class WolfContainer : public litehtml::document_container
{
  public:
    struct ImageEntry
    {
        std::string           key;
        int                   width = 0;
        int                   height = 0;
        std::vector<uint8_t>  pixels;
    };

    WolfContainer(int width, int height);

    /// How this document treats <img> sources.
    enum ImageMode
    {
        /// Reuse the shared cache only; a tab switch must never block.
        images_cache_only = 0,
        /// Fetch during layout (legacy path, blocks the browser task).
        images_fetch_now = 1,
        /// Record the URLs; the browser fetches them asynchronously.
        images_collect = 2,
    };

    void set_image_mode(int mode)
    {
        m_image_mode = mode;
    }

    /// Pops the next image source that still needs a network fetch.
    bool pop_pending_image(std::string& out);

    /// Decodes `data` and stores it in the shared cache. Called by Rust when
    /// an asynchronous image request completes.
    void store_image(const char* src, const uint8_t* data, int len);

    /// How many more images this document may add to the shared cache.
    int image_slots() const;

    void set_canvas(WolfCanvas* canvas)
    {
        m_canvas = canvas;
    }
    void set_viewport(int width, int height)
    {
        m_width  = width;
        m_height = height;
    }
    /// URL collected by `on_anchor_click`; empty when the last click was not
    /// on a link.
    void clear_link()
    {
        m_link.clear();
    }
    const std::string& link() const
    {
        return m_link;
    }

    // litehtml::document_container
    litehtml::uint_ptr create_font(const litehtml::font_description& descr, const litehtml::document* doc,
                                   litehtml::font_metrics* fm) override;
    void               delete_font(litehtml::uint_ptr hFont) override;
    litehtml::pixel_t  text_width(const char* text, litehtml::uint_ptr hFont) override;
    void               draw_text(litehtml::uint_ptr hdc, const char* text, litehtml::uint_ptr hFont,
                                 litehtml::web_color color, const litehtml::position& pos) override;
    litehtml::pixel_t  pt_to_px(float pt) const override;
    litehtml::pixel_t  get_default_font_size() const override;
    const char*        get_default_font_name() const override;
    void               draw_list_marker(litehtml::uint_ptr hdc, const litehtml::list_marker& marker) override;
    void               load_image(const char* src, const char* baseurl, bool redraw_on_ready) override;
    void               get_image_size(const char* src, const char* baseurl, litehtml::size& sz) override;
    void               draw_image(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                  const std::string& url, const std::string& base_url) override;
    void               draw_solid_fill(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                       const litehtml::web_color& color) override;
    void               draw_linear_gradient(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                            const litehtml::background_layer::linear_gradient& gradient) override;
    void               draw_radial_gradient(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                            const litehtml::background_layer::radial_gradient& gradient) override;
    void               draw_conic_gradient(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                          const litehtml::background_layer::conic_gradient& gradient) override;
    void               draw_borders(litehtml::uint_ptr hdc, const litehtml::borders& borders,
                                    const litehtml::position& draw_pos, bool root) override;

    void set_caption(const char* caption) override;
    void set_base_url(const char* base_url) override;
    void link(const std::shared_ptr<litehtml::document>& doc, const litehtml::element::ptr& el) override;
    void on_anchor_click(const char* url, const litehtml::element::ptr& el) override;
    void on_mouse_event(const litehtml::element::ptr& el, litehtml::mouse_event event) override;
    void set_cursor(const char* cursor) override;
    void transform_text(std::string& text, litehtml::text_transform tt) override;
    void import_css(std::string& text, const std::string& url, std::string& baseurl) override;
    void set_clip(const litehtml::position& pos, const litehtml::border_radiuses& bdr_radius) override;
    void del_clip() override;
    void get_viewport(litehtml::position& viewport) const override;
    litehtml::element::ptr create_element(const char* tag_name, const litehtml::string_map& attributes,
                                          const std::shared_ptr<litehtml::document>& doc) override;

    void get_media_features(litehtml::media_features& media) const override;
    void get_language(std::string& language, std::string& culture) const override;

  private:
    int         m_width;
    int         m_height;
    WolfCanvas* m_canvas = nullptr;
    std::string m_base_url;
    std::string m_link;
    int  m_image_fetches = 0;
    int  m_image_mode = images_fetch_now;
    std::vector<std::string> m_pending_images;

    std::string image_key(const char* src, const char* baseurl) const;
    ImageEntry* find_image(const std::string& key);
    bool        decode_store(const std::string& key, const uint8_t* data, int len);
};

#endif // WOLF_CONTAINER_H
