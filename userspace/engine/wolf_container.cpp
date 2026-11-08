#include "wolf_container.h"

#include "stb_image.h"

#include <algorithm>
#include <cctype>
#include <cmath>
#include <cstring>

namespace
{
    std::vector<WolfContainer::ImageEntry>& image_cache()
    {
        // One browser task, one document at a time. Keeping the RGBA cache
        // across document rebuilds makes tab switches instant instead of
        // re-fetching every <img> synchronously.
        static std::vector<WolfContainer::ImageEntry> cache;
        return cache;
    }

    // The ivshmem surface is consumed as QImage::Format_RGBA8888, so a
    // native little-endian uint32 is 0xAABBGGRR, not 0xAARRGGBB.
    inline uint32_t argb(const litehtml::web_color& color)
    {
        return (static_cast<uint32_t>(color.alpha) << 24) |
               (static_cast<uint32_t>(color.blue) << 16) |
               (static_cast<uint32_t>(color.green) << 8) |
               static_cast<uint32_t>(color.red);
    }

    inline uint32_t blend(uint32_t dst, uint32_t src)
    {
        const uint32_t alpha = src >> 24;
        if(alpha == 0)
        {
            return dst;
        }
        if(alpha == 255)
        {
            return src;
        }
        const uint32_t inverse = 255 - alpha;
        const uint32_t red   = ((src & 0xFF) * alpha + (dst & 0xFF) * inverse) / 255;
        const uint32_t green = (((src >> 8) & 0xFF) * alpha + ((dst >> 8) & 0xFF) * inverse) / 255;
        const uint32_t blue  = (((src >> 16) & 0xFF) * alpha + ((dst >> 16) & 0xFF) * inverse) / 255;
        return 0xFF000000u | (blue << 16) | (green << 8) | red;
    }

    inline void put(WolfCanvas* canvas, int x, int y, uint32_t color)
    {
        if(canvas == nullptr || canvas->pixels == nullptr)
        {
            return;
        }
        if(x < 0 || y < 0 || x >= canvas->width || y >= canvas->height)
        {
            return;
        }
        if(canvas->has_clip &&
           (x < canvas->clip_x || y < canvas->clip_y || x >= canvas->clip_x + canvas->clip_w ||
            y >= canvas->clip_y + canvas->clip_h))
        {
            return;
        }
        uint32_t* pixel = canvas->pixels + static_cast<size_t>(y) * canvas->stride + x;
        *pixel          = blend(*pixel, color);
    }

    void fill_rect(WolfCanvas* canvas, int x, int y, int width, int height, uint32_t color)
    {
        if(canvas == nullptr || canvas->pixels == nullptr || width <= 0 || height <= 0)
        {
            return;
        }
        int left   = std::max(x, 0);
        int top    = std::max(y, 0);
        int right  = std::min(x + width, canvas->width);
        int bottom = std::min(y + height, canvas->height);
        if(canvas->has_clip)
        {
            left   = std::max(left, canvas->clip_x);
            top    = std::max(top, canvas->clip_y);
            right  = std::min(right, canvas->clip_x + canvas->clip_w);
            bottom = std::min(bottom, canvas->clip_y + canvas->clip_h);
        }
        for(int row = top; row < bottom; ++row)
        {
            uint32_t* line = canvas->pixels + static_cast<size_t>(row) * canvas->stride;
            for(int column = left; column < right; ++column)
            {
                line[column] = blend(line[column], color);
            }
        }
    }

    void fill_box(WolfCanvas* canvas, const litehtml::position& box, uint32_t color)
    {
        fill_rect(canvas, static_cast<int>(box.x), static_cast<int>(box.y),
                  static_cast<int>(box.width), static_cast<int>(box.height), color);
    }

    void draw_rgba(WolfCanvas* canvas, const std::vector<uint8_t>& pixels, int source_width,
                   int source_height, int x, int y, int width, int height,
                   const litehtml::position& clip)
    {
        if(canvas == nullptr || source_width <= 0 || source_height <= 0 || width <= 0 || height <= 0)
        {
            return;
        }
        const int clip_left   = static_cast<int>(clip.x);
        const int clip_top    = static_cast<int>(clip.y);
        const int clip_right  = clip_left + static_cast<int>(clip.width);
        const int clip_bottom = clip_top + static_cast<int>(clip.height);
        for(int row = 0; row < height; ++row)
        {
            const int source_y = row * source_height / height;
            for(int column = 0; column < width; ++column)
            {
                const int pixel_x = x + column;
                const int pixel_y = y + row;
                if(pixel_x < clip_left || pixel_y < clip_top || pixel_x >= clip_right ||
                   pixel_y >= clip_bottom)
                {
                    continue;
                }
                const int source_x = column * source_width / width;
                const size_t offset =
                    (static_cast<size_t>(source_y) * source_width + source_x) * 4;
                if(offset + 4 > pixels.size())
                {
                    continue;
                }
                const uint8_t red   = pixels[offset];
                const uint8_t green = pixels[offset + 1];
                const uint8_t blue  = pixels[offset + 2];
                const uint8_t alpha = pixels[offset + 3];
                if(alpha == 0)
                {
                    continue;
                }
                const uint32_t color = (static_cast<uint32_t>(alpha) << 24) |
                                       (static_cast<uint32_t>(blue) << 16) |
                                       (static_cast<uint32_t>(green) << 8) |
                                       static_cast<uint32_t>(red);
                put(canvas, pixel_x, pixel_y, color);
            }
        }
    }
} // namespace

WolfContainer::WolfContainer(int width, int height) :
    m_width(width),
    m_height(height)
{
}

litehtml::uint_ptr WolfContainer::create_font(const litehtml::font_description& descr,
                                             const litehtml::document* /*doc*/,
                                             litehtml::font_metrics* fm)
{
    // The surface has one 5x7 font. CSS pixel sizes are mapped to an integer
    // scale: 8px -> 1, 16px -> 2, 24px -> 3, 32px -> 4. Larger sizes clamp,
    // because a 5x7 glyph at scale > 4 is unreadable and wastes the viewport.
    int scale = static_cast<int>(std::lround(static_cast<float>(descr.size) / 8.0f));
    scale     = std::clamp(scale, 1, 4);

    if(fm != nullptr)
    {
        fm->font_size   = descr.size;
        fm->height      = 8 * scale;
        fm->ascent      = 7 * scale;
        fm->descent     = 1 * scale;
        fm->x_height    = 5 * scale;
        fm->ch_width    = 6 * scale;
        fm->draw_spaces = true;
        fm->sub_shift   = scale;
        fm->super_shift = -2 * scale;
    }
    return static_cast<litehtml::uint_ptr>(scale);
}

void WolfContainer::delete_font(litehtml::uint_ptr /*hFont*/)
{
}

litehtml::pixel_t WolfContainer::text_width(const char* text, litehtml::uint_ptr hFont)
{
    if(text == nullptr)
    {
        return 0;
    }
    const uint32_t scale = static_cast<uint32_t>(hFont);
    const uint32_t width = wolf_text_width(text, static_cast<uint32_t>(std::strlen(text)), scale);
    return litehtml::pixel_t(static_cast<float>(width));
}

void WolfContainer::draw_text(litehtml::uint_ptr hdc, const char* text, litehtml::uint_ptr hFont,
                              litehtml::web_color color, const litehtml::position& pos)
{
    auto* canvas = reinterpret_cast<WolfCanvas*>(hdc);
    if(canvas == nullptr || canvas->pixels == nullptr || text == nullptr || *text == '\0')
    {
        return;
    }
    const int clip_x = canvas->has_clip ? canvas->clip_x : 0;
    const int clip_y = canvas->has_clip ? canvas->clip_y : 0;
    const int clip_w = canvas->has_clip ? canvas->clip_w : canvas->width;
    const int clip_h = canvas->has_clip ? canvas->clip_h : canvas->height;
    wolf_draw_text(canvas->pixels, canvas->stride, canvas->width, canvas->height,
                   static_cast<int>(pos.x), static_cast<int>(pos.y), text,
                   static_cast<uint32_t>(std::strlen(text)), static_cast<uint32_t>(hFont),
                   argb(color), clip_x, clip_y, clip_w, clip_h);
}

litehtml::pixel_t WolfContainer::pt_to_px(float pt) const
{
    return static_cast<litehtml::pixel_t>(pt * 96.0f / 72.0f);
}

litehtml::pixel_t WolfContainer::get_default_font_size() const
{
    return 16;
}

const char* WolfContainer::get_default_font_name() const
{
    return "Wolfser";
}

void WolfContainer::draw_list_marker(litehtml::uint_ptr hdc, const litehtml::list_marker& marker)
{
    auto* canvas = reinterpret_cast<WolfCanvas*>(hdc);
    if(canvas == nullptr)
    {
        return;
    }
    switch(marker.marker_type)
    {
    case litehtml::list_style_type_circle:
    case litehtml::list_style_type_disc:
    case litehtml::list_style_type_square:
        fill_rect(canvas, static_cast<int>(marker.pos.x), static_cast<int>(marker.pos.y) + 2,
                  std::max(2, static_cast<int>(marker.pos.width)),
                  std::max(2, static_cast<int>(marker.pos.height)), argb(marker.color));
        break;
    default:
    {
        // Decimal and the alpha/roman variants all fall back to an ASCII
        // number. A real engine would use the CSS counter style; this is
        // enough to keep ordered lists readable.
        std::string number = std::to_string(marker.index > 0 ? marker.index : 1);
        number += '.';
        const int clip_x = canvas->has_clip ? canvas->clip_x : 0;
        const int clip_y = canvas->has_clip ? canvas->clip_y : 0;
        const int clip_w = canvas->has_clip ? canvas->clip_w : canvas->width;
        const int clip_h = canvas->has_clip ? canvas->clip_h : canvas->height;
        wolf_draw_text(canvas->pixels, canvas->stride, canvas->width, canvas->height,
                       static_cast<int>(marker.pos.x), static_cast<int>(marker.pos.y), number.c_str(),
                       static_cast<uint32_t>(number.size()), static_cast<uint32_t>(marker.font),
                       argb(marker.color), clip_x, clip_y, clip_w, clip_h);
        break;
    }
    }
}

std::string WolfContainer::image_key(const char* src, const char* /*baseurl*/) const
{
    // litehtml passes nullptr for <img> base URLs, so use the document base
    // remembered from set_base_url. This still shares images between the
    // original render and a tab-switch rebuild, while distinguishing the same
    // relative path on different sites.
    std::string key = m_base_url;
    key += '|';
    key += src != nullptr ? src : "";
    return key;
}

WolfContainer::ImageEntry* WolfContainer::find_image(const std::string& key)
{
    for(auto& image : image_cache())
    {
        if(image.key == key)
        {
            return &image;
        }
    }
    return nullptr;
}

void WolfContainer::load_image(const char* src, const char* baseurl, bool /*redraw_on_ready*/)
{
    if(src == nullptr || *src == '\0')
    {
        return;
    }
    const std::string key = image_key(src, baseurl);
    if(find_image(key) != nullptr)
    {
        return;
    }
    if(!m_allow_image_fetch || m_image_fetches >= 4 || image_cache().size() >= 4)
    {
        return;
    }
    // Wikipedia thumbnails are tens of KiB; 512 KiB also covers most direct
    // PNG/JPEG links while keeping the net_server window and heap bounded.
    constexpr uint32_t kEncodedLimit = 512 * 1024;
    std::vector<uint8_t> encoded(kEncodedLimit);
    // litehtml passes nullptr for <img> sources and expects the container to
    // remember the document base URL from set_base_url().
    const char* base = (baseurl != nullptr && *baseurl != '\0')
                           ? baseurl
                           : m_base_url.c_str();
    const int32_t length = wolf_fetch_image(
        reinterpret_cast<const uint8_t*>(src), static_cast<uint32_t>(std::strlen(src)),
        reinterpret_cast<const uint8_t*>(base), static_cast<uint32_t>(std::strlen(base)),
        encoded.data(), kEncodedLimit);
    ++m_image_fetches;
    if(length <= 0)
    {
        return;
    }

    int width = 0;
    int height = 0;
    int channels = 0;
    if(stbi_info_from_memory(encoded.data(), length, &width, &height, &channels) == 0)
    {
        return;
    }
    if(width <= 0 || height <= 0 ||
       static_cast<int64_t>(width) * height > 300 * 1000)
    {
        return;
    }
    // Favicons, tracking pixels and spacers are not worth a cache slot.
    if(width < 32 || height < 32)
    {
        return;
    }
    unsigned char* rgba =
        stbi_load_from_memory(encoded.data(), length, &width, &height, &channels, 4);
    if(rgba == nullptr)
    {
        return;
    }
    ImageEntry entry;
    entry.key    = key;
    entry.width  = width;
    entry.height = height;
    entry.pixels.assign(rgba, rgba + static_cast<size_t>(width) * height * 4);
    stbi_image_free(rgba);
    image_cache().push_back(std::move(entry));
}

void WolfContainer::get_image_size(const char* src, const char* baseurl, litehtml::size& sz)
{
    sz = litehtml::size(0, 0);
    if(src == nullptr || *src == '\0')
    {
        return;
    }
    ImageEntry* image = find_image(image_key(src, baseurl));
    if(image == nullptr)
    {
        // litehtml may ask for the intrinsic size before it starts loading;
        // make the fetch synchronous here so the layout reserves real space.
        load_image(src, baseurl, false);
        image = find_image(image_key(src, baseurl));
    }
    if(image != nullptr)
    {
        sz.width  = image->width;
        sz.height = image->height;
    }
}

void WolfContainer::draw_image(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                               const std::string& url, const std::string& base_url)
{
    auto* canvas = reinterpret_cast<WolfCanvas*>(hdc);
    if(canvas == nullptr)
    {
        return;
    }
    ImageEntry* image = find_image(image_key(url.c_str(), base_url.c_str()));
    if(image == nullptr)
    {
        return;
    }
    int width  = static_cast<int>(layer.origin_box.width);
    int height = static_cast<int>(layer.origin_box.height);
    if(width <= 0 || height <= 0)
    {
        width  = image->width;
        height = image->height;
    }
    draw_rgba(canvas, image->pixels, image->width, image->height,
              static_cast<int>(layer.origin_box.x), static_cast<int>(layer.origin_box.y),
              width, height, layer.clip_box);
}

void WolfContainer::draw_solid_fill(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                    const litehtml::web_color& color)
{
    fill_box(reinterpret_cast<WolfCanvas*>(hdc), layer.border_box, argb(color));
}

void WolfContainer::draw_linear_gradient(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                         const litehtml::background_layer::linear_gradient& gradient)
{
    const uint32_t color = gradient.color_points.empty() ? 0xFFFFFFFFu : argb(gradient.color_points.front().color);
    fill_box(reinterpret_cast<WolfCanvas*>(hdc), layer.border_box, color);
}

void WolfContainer::draw_radial_gradient(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                         const litehtml::background_layer::radial_gradient& gradient)
{
    const uint32_t color = gradient.color_points.empty() ? 0xFFFFFFFFu : argb(gradient.color_points.front().color);
    fill_box(reinterpret_cast<WolfCanvas*>(hdc), layer.border_box, color);
}

void WolfContainer::draw_conic_gradient(litehtml::uint_ptr hdc, const litehtml::background_layer& layer,
                                        const litehtml::background_layer::conic_gradient& gradient)
{
    const uint32_t color = gradient.color_points.empty() ? 0xFFFFFFFFu : argb(gradient.color_points.front().color);
    fill_box(reinterpret_cast<WolfCanvas*>(hdc), layer.border_box, color);
}

void WolfContainer::draw_borders(litehtml::uint_ptr hdc, const litehtml::borders& borders,
                                 const litehtml::position& pos, bool /*root*/)
{
    auto* canvas = reinterpret_cast<WolfCanvas*>(hdc);
    if(canvas == nullptr)
    {
        return;
    }
    const int x = static_cast<int>(pos.x);
    const int y = static_cast<int>(pos.y);
    const int w = static_cast<int>(pos.width);
    const int h = static_cast<int>(pos.height);
    const float top_width    = static_cast<float>(borders.top.width);
    const float bottom_width = static_cast<float>(borders.bottom.width);
    const float left_width   = static_cast<float>(borders.left.width);
    const float right_width  = static_cast<float>(borders.right.width);
    if(borders.top.style != litehtml::border_style_none && top_width > 0.0f)
    {
        fill_rect(canvas, x, y, w, static_cast<int>(top_width), argb(borders.top.color));
    }
    if(borders.bottom.style != litehtml::border_style_none && bottom_width > 0.0f)
    {
        fill_rect(canvas, x, y + h - static_cast<int>(bottom_width), w,
                  static_cast<int>(bottom_width), argb(borders.bottom.color));
    }
    if(borders.left.style != litehtml::border_style_none && left_width > 0.0f)
    {
        fill_rect(canvas, x, y, static_cast<int>(left_width), h, argb(borders.left.color));
    }
    if(borders.right.style != litehtml::border_style_none && right_width > 0.0f)
    {
        fill_rect(canvas, x + w - static_cast<int>(right_width), y,
                  static_cast<int>(right_width), h, argb(borders.right.color));
    }
}

void WolfContainer::set_caption(const char* /*caption*/)
{
}

void WolfContainer::set_base_url(const char* base_url)
{
    m_base_url = base_url != nullptr ? base_url : "";
}

void WolfContainer::link(const std::shared_ptr<litehtml::document>& /*doc*/, const litehtml::element::ptr& /*el*/)
{
}

void WolfContainer::on_anchor_click(const char* url, const litehtml::element::ptr& /*el*/)
{
    m_link = url != nullptr ? url : "";
}

void WolfContainer::on_mouse_event(const litehtml::element::ptr& /*el*/, litehtml::mouse_event /*event*/)
{
}

void WolfContainer::set_cursor(const char* /*cursor*/)
{
}

void WolfContainer::transform_text(std::string& text, litehtml::text_transform tt)
{
    switch(tt)
    {
    case litehtml::text_transform_uppercase:
        for(char& character : text)
        {
            character = static_cast<char>(std::toupper(static_cast<unsigned char>(character)));
        }
        break;
    case litehtml::text_transform_lowercase:
        for(char& character : text)
        {
            character = static_cast<char>(std::tolower(static_cast<unsigned char>(character)));
        }
        break;
    case litehtml::text_transform_capitalize:
    {
        bool start = true;
        for(char& character : text)
        {
            if(std::isalpha(static_cast<unsigned char>(character)))
            {
                if(start)
                {
                    character = static_cast<char>(std::toupper(static_cast<unsigned char>(character)));
                    start     = false;
                }
            }
            else if(character == ' ' || character == '\t' || character == '\n')
            {
                start = true;
            }
        }
        break;
    }
    default:
        break;
    }
}

void WolfContainer::import_css(std::string& text, const std::string& /*url*/, std::string& /*baseurl*/)
{
    // External stylesheets are not fetched yet. Leaving `text` empty lets the
    // page keep litehtml's master CSS and any inline <style> block.
    text.clear();
}

void WolfContainer::set_clip(const litehtml::position& pos, const litehtml::border_radiuses& /*bdr_radius*/)
{
    if(m_canvas == nullptr)
    {
        return;
    }
    m_canvas->clip_stack.push_back(litehtml::position(m_canvas->clip_x, m_canvas->clip_y,
                                                      m_canvas->clip_w, m_canvas->clip_h));
    const int x = static_cast<int>(pos.x);
    const int y = static_cast<int>(pos.y);
    const int w = static_cast<int>(pos.width);
    const int h = static_cast<int>(pos.height);
    if(!m_canvas->has_clip)
    {
        m_canvas->clip_x   = x;
        m_canvas->clip_y   = y;
        m_canvas->clip_w   = w;
        m_canvas->clip_h   = h;
        m_canvas->has_clip = true;
        return;
    }
    const int left   = std::max(m_canvas->clip_x, x);
    const int top    = std::max(m_canvas->clip_y, y);
    const int right  = std::min(m_canvas->clip_x + m_canvas->clip_w, x + w);
    const int bottom = std::min(m_canvas->clip_y + m_canvas->clip_h, y + h);
    m_canvas->clip_x = left;
    m_canvas->clip_y = top;
    m_canvas->clip_w = std::max(0, right - left);
    m_canvas->clip_h = std::max(0, bottom - top);
}

void WolfContainer::del_clip()
{
    if(m_canvas == nullptr || m_canvas->clip_stack.empty())
    {
        return;
    }
    const litehtml::position previous = m_canvas->clip_stack.back();
    m_canvas->clip_stack.pop_back();
    m_canvas->clip_x = static_cast<int>(previous.x);
    m_canvas->clip_y = static_cast<int>(previous.y);
    m_canvas->clip_w = static_cast<int>(previous.width);
    m_canvas->clip_h = static_cast<int>(previous.height);
}

void WolfContainer::get_viewport(litehtml::position& viewport) const
{
    viewport = litehtml::position(0, 0, m_width, m_height);
}

litehtml::element::ptr WolfContainer::create_element(const char* /*tag_name*/,
                                                     const litehtml::string_map& /*attributes*/,
                                                     const std::shared_ptr<litehtml::document>& /*doc*/)
{
    return nullptr;
}

void WolfContainer::get_media_features(litehtml::media_features& media) const
{
    media.type          = litehtml::media_type_screen;
    media.width         = m_width;
    media.height        = m_height;
    media.device_width  = m_width;
    media.device_height = m_height;
    media.color         = 8;
    media.color_index   = 0;
    media.monochrome    = 0;
    media.resolution    = 96;
}

void WolfContainer::get_language(std::string& language, std::string& culture) const
{
    language = "en";
    culture  = "US";
}
