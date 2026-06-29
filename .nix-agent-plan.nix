# nix-agent plan v1
# plan-id: 2026-06-29-tmux-vi
# prompt: add tmux with vi-style keybindings

{
  environment.systemPackages = with pkgs; [
    tmux
  ];

  services.tmux = {
    enable = true;
    keybindings = {
      viMode = true;
    };
  };
}
