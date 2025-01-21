if vim.g.neovide then
  local cursor_vfx = { "railgun", "torpedo", "pixiedust", "sonicboom", "ripple", "wireframe" }
  math.randomseed(os.time())

  vim.g.neovide_cursor_vfx_mode = cursor_vfx[math.random(1, #cursor_vfx)]

  vim.keymap.set("n", "<F11>", function()
    vim.g.neovide_fullscreen = not vim.g.neovide_fullscreen
  end, {})


  vim.g.neovide_scale_factor = 1.0
  vim.g.neovide_scroll_animation_length = 0.615
  vim.g.neovide_floating_blur_amount_x = 10.0
  vim.g.neovide_floating_blur_amount_y = 10.0
  vim.g.neovide_transparency = 1.0
  vim.g.neovide_refresh_rate = 144
  vim.g.neovide_hide_mouse_when_typing = true
  vim.g.neovide_confirm_quit = true
  vim.g.neovide_floating_corner_radius = 0.5

  vim.g.neovide_padding_top = 20
  -- vim.g.neovide_padding_bottom = 0
  -- vim.g.neovide_padding_right = 40
  -- vim.g.neovide_padding_left = 40
  vim.g.neovide_padding_left = 20

  vim.o.laststatus = 0
  vim.o.cmdheight = 0
  vim.o.guifont = "CodeNewRoman Nerd Font Mono:h20"
  -- vim.o.guifont = "IBM PLex Mono:h18"

  vim.cmd([[
    colorscheme lunaperche
    set background=dark
    terminal
    startinsert
    set scrolloff=9999
    autocmd BufLeave term://* quit
  ]])
end
