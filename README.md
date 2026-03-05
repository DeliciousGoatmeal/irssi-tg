# ~*~ xX_Rust-TG-CLI_Xx ~*~

> *"Welcome to my project page! Please sign the guestbook! ^_^"*

![Hit Counter](https://img.shields.io/badge/Hits-999999-blue?style=plastic) ![Best Viewed In](https://img.shields.io/badge/Best_Viewed_In-Netscape_Navigator-008000?style=plastic) ![Webring](https://img.shields.io/badge/Webring-H4x0rs_Only-black?style=plastic)

Sup peeps! Welcome to my sick new project. Are you tired of totally n00bish, RAM-hogging GUI apps? Do you miss the days of hanging out on IRC while downloading MP3s on Kazaa? Well, grab a Mountain Dew and check this out.

I coded this entirely in Rust. It's an `irssi`-style terminal client for Telegram. It is totally l33t and will make you look like a straight-up h4x0r to everyone in your dorm room. w00t! xD

<hr>

## 🌟 ~ F34tur3s ~ 🌟

* **Totally CLI:** No mouse needed. Only n00bs use mice. O_o
* **Irssi Themes:** Comes with sick themes like `efnet`, `matrix`, and `dracula`.
* **Tab Completion:** Because typing out full usernames is an epic fail.
* **Workspace Persistence:** Remembers your open tabs between sessions. ROFLMAO!
* **Trout Slapping:** Type `/trout <name>` to slap someone with a large trout. (Classic!)

<hr>

## 💿 ~ Inst4ll4ti0n ~ 💿

First, you gotta clone this repo to your rig. Open up your terminal (bash, cmd, whatever - just don't use WebTV lol) and type:

```bash
git clone [https://github.com/your-username/xX_rust-tg-cli_Xx.git](https://github.com/your-username/xX_rust-tg-cli_Xx.git)
cd xX_rust-tg-cli_Xx
```
Next, you need your API keys. Go to the Telegram dev website and get your api_id and api_hash. Keep these secret! If u share them u will get pwned. >_<Make a hidden file called .env in the root folder like this:Code snippet
```
TG_API_ID=1337
TG_API_HASH=ur_secret_hash_goes_here_xD
```
Then, just compile it! (Warning: might take a min on a dial-up connection jk)Bashcargo build --release
```
./target/release/rust-tg-cli --theme dracula
```
<hr>🎮 ~ H0w 2 Us3 ~ 🎮Once you log in (it sends a code to your phone, pretty high-tech stuff), you'll see the command prompt.Here are the l33t commands u need to know to rule the chatroom:CommandWut it does/chats or /listShows u who u can talk to./join <name>Opens a chat window. PROTIP: hit <TAB> to auto-complete!/msg <name> <msg>Whispers a secret message to someone. shhh. :P/win <number>Switches between your open windows. /win 1 goes back to status./closeCloses your current tab./whois <name>Gets the 411 on a user. A/S/L?/trout <name>Trolls them with a fish./quitG2G, TTYL!<hr>🎨 ~ C0st0m1z3 (Th3m3s) ~ 🎨If the default colors are too boring, u can edit the themes.ini file! You can use hex codes just like styling your MySpace profile. It auto-generates the first time u run the app.Example:Ini, TOML[my_sweet_theme]
bar_bg = #000000
bar_fg = #00FF00
sys_msg = #FF00FF
text_fg = Reset
nick_colors = Cyan, Green, Yellow

To use it, just launch the app with: rust-tg-cli --theme my_sweet_theme<hr>🤝 ~ Guestb00k & C0ntributi0ns ~ 🤝If u find a bug, don't flame me on the message boards! Just open a pull request. I code this after school so give me time to review it.Drop a star on this repo if you think it's totally pwnage. 

<3~ Copyleft 2004-2024. All your codebase are belong to us. ~
