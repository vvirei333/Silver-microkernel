// C ABI around one litehtml document.
//
// Wolfser owns the fetch and the surface; this file owns the document
// lifetime, layout and hit-testing. The ABI is deliberately small and uses
// only plain C types so the Rust side does not need C++ bindings.

#include "wolf_container.h"

#include <cstring>
#include <new>
#include <string>

extern "C" void __libc_init_array(void);
extern "C" void wolf_sys_exit(uint64_t status) __attribute__((noreturn));


namespace
{
    struct WolfHtml
    {
        litehtml::document::ptr doc;
        WolfContainer*         container = nullptr;
        int                    width = 0;
        int                    height = 0;
        std::string            base_url;
        std::string            link;
    };

    void no_redraw(const litehtml::position&)
    {
    }

    const char* find_href(const litehtml::element* element)
    {
        for(const litehtml::element* current = element; current != nullptr;
            current = current->parent().get())
        {
            const char* href = current->get_attr("href");
            if(href != nullptr && *href != '\0')
            {
                return href;
            }
        }
        return nullptr;
    }
} // namespace

extern "C"
{
    void wolf_cpp_init(void)
    {
        static bool initialized = false;
        if(!initialized)
        {
            initialized = true;
            __libc_init_array();
        }
    }

    WolfHtml* wolf_html_create(const char* html, int len, int width, int height,
                               const char* base_url, int allow_images)
    {
        wolf_cpp_init();
        if(html == nullptr || len <= 0 || width <= 0 || height <= 0)
        {
            return nullptr;
        }
        auto* handle = new (std::nothrow) WolfHtml();
        if(handle == nullptr)
        {
            return nullptr;
        }
        handle->width    = width;
        handle->height   = height;
        handle->base_url = base_url != nullptr ? base_url : "";
        handle->container = new (std::nothrow) WolfContainer(width, height);
        if(handle->container == nullptr)
        {
            delete handle;
            return nullptr;
        }
        handle->container->set_base_url(handle->base_url.c_str());
        handle->container->set_allow_image_fetch(allow_images != 0);

        try
        {
            litehtml::estring text(std::string(html, static_cast<size_t>(len)));
            handle->doc = litehtml::document::createFromString(text, handle->container);
        }
        catch(...)
        {
            handle->doc = nullptr;
        }
        if(!handle->doc)
        {
            delete handle->container;
            delete handle;
            return nullptr;
        }
        handle->doc->render(width);
        handle->height = static_cast<int>(handle->doc->height());
        return handle;
    }

    void wolf_html_destroy(WolfHtml* handle)
    {
        if(handle == nullptr)
        {
            return;
        }
        handle->doc.reset();
        delete handle->container;
        delete handle;
    }

    int wolf_html_height(WolfHtml* handle)
    {
        return handle != nullptr ? handle->height : 0;
    }

    int wolf_html_width(WolfHtml* handle)
    {
        return handle != nullptr ? static_cast<int>(handle->doc->width()) : 0;
    }

    void wolf_html_draw(WolfHtml* handle, uint32_t* pixels, int stride, int width, int height,
                        int scroll_y)
    {
        if(handle == nullptr || handle->doc == nullptr || pixels == nullptr || width <= 0 || height <= 0)
        {
            return;
        }
        WolfCanvas canvas;
        canvas.pixels   = pixels;
        canvas.width    = width;
        canvas.height   = height;
        canvas.stride   = stride;
        canvas.has_clip = true;
        canvas.clip_x   = 0;
        canvas.clip_y   = 0;
        canvas.clip_w   = width;
        canvas.clip_h   = height;

        handle->container->set_canvas(&canvas);
        // litehtml compares the clip against the *screen* bounds after adding
        // the draw offset, so the visible viewport is (0,0,width,height) even
        // when the document is scrolled.
        litehtml::position clip(0, 0, width, height);
        handle->doc->draw(reinterpret_cast<litehtml::uint_ptr>(&canvas), 0, -scroll_y, &clip);
        handle->container->set_canvas(nullptr);
    }

    /// Returns non-null when the point is over a link. The returned string
    /// stays valid until the next hover/click/destroy call.
    const char* wolf_html_hover(WolfHtml* handle, int x, int y, int scroll_y, int* length)
    {
        if(length != nullptr)
        {
            *length = 0;
        }
        if(handle == nullptr || handle->doc == nullptr)
        {
            return nullptr;
        }
        handle->doc->on_mouse_over(x, y + scroll_y, x, y, no_redraw);
        const litehtml::element::const_ptr over = handle->doc->get_over_element();
        const char* href = find_href(over.get());
        if(href == nullptr)
        {
            return nullptr;
        }
        handle->link = href;
        if(length != nullptr)
        {
            *length = static_cast<int>(handle->link.size());
        }
        return handle->link.c_str();
    }

    /// Dispatches a left click and returns the anchor URL, if any.
    const char* wolf_html_click(WolfHtml* handle, int x, int y, int scroll_y, int* length)
    {
        if(length != nullptr)
        {
            *length = 0;
        }
        if(handle == nullptr || handle->doc == nullptr)
        {
            return nullptr;
        }
        handle->container->clear_link();
        handle->doc->on_lbutton_down(x, y + scroll_y, x, y, no_redraw);
        handle->doc->on_lbutton_up(x, y + scroll_y, x, y, no_redraw);
        handle->link = handle->container->link();
        if(handle->link.empty())
        {
            return nullptr;
        }
        if(length != nullptr)
        {
            *length = static_cast<int>(handle->link.size());
        }
        return handle->link.c_str();
    }

    const char* wolf_html_base(WolfHtml* handle, int* length)
    {
        if(length != nullptr)
        {
            *length = 0;
        }
        if(handle == nullptr)
        {
            return nullptr;
        }
        if(length != nullptr)
        {
            *length = static_cast<int>(handle->base_url.size());
        }
        return handle->base_url.c_str();
    }
}
