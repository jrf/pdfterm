repeat 15 times
  if not (exists terminal id (item 1 of argv)) then
    -- Collapsing an unfocused split swaps SwiftUI's subtree identity and drops firstResponder.
    try
      focus (focused terminal of selected tab of front window)
    end try
    return
  end if
  -- Raw ETX quits the reader under Kitty keyboard protocol; the key event dismisses any retained exit screen.
  try
    input text (character id 3) to terminal id (item 1 of argv)
    send key "c" modifiers "control" to terminal id (item 1 of argv)
  end try
  delay 0.02
end repeat
if exists terminal id (item 1 of argv) then
  try
    close terminal id (item 1 of argv)
  end try
  repeat 25 times
    if not (exists terminal id (item 1 of argv)) then
      try
        focus (focused terminal of selected tab of front window)
      end try
      return
    end if
    delay 0.02
  end repeat
end if
if exists terminal id (item 1 of argv) then error "owned viewer did not exit"
