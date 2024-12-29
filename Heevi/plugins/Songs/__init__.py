from nonebot import on_command,logger
from typing import Union
from nonebot.rule import to_me
from nonebot.matcher import Matcher
from nonebot.adapters.onebot.v11 import Message,Bot,MessageSegment,GroupMessageEvent,PrivateMessageEvent
from nonebot.params import CommandArg,ArgPlainText
from nonebot.typing import T_State
from nonebot.exception import MatcherException
from .get_song import Song_Get as ncm

Song = on_command("点歌", rule=to_me(), priority=10, block=True)

@Song.handle()
async def GetSong(matcher:Matcher,arg:Message = CommandArg()):
    if arg.extract_plain_text():
        matcher.set_arg("SongName",arg)

@Song.got("SongName",prompt="想点哪首歌呢")
async def SearchSong(bot:Bot,state : T_State,SongName=ArgPlainText(),event = Union[GroupMessageEvent,PrivateMessageEvent]):
    try :
        state['Songname'] = SongName
        SongList = ncm.ncm_song_search(SongName)
        if SongList['code'] == 200 :
            logger.success("获取歌曲成功，正在生成选歌菜单")
            SongsGet = SongList['data']
            Songs = [{"index": song["n"], "title": song["title"], "singer": song["singer"]} for song in SongsGet]
            Menu_Card = "🎵 点歌菜单 🎵\n"
            for song in Songs :
                Menu_Card += f"{song['index']}.《{song['title']}》 - {song['singer']}\n"
            await bot.send(event=event,message=Menu_Card)
        else :
            logger.error(f"获取失败，状态码为{SongList['code']}")
            await Song.finish("获取歌曲失败，请查看后台或联系管理员")
    except Exception as e :
        logger.error(f"获取歌曲时出现错误，错误为{e}")
        await Song.finish("获取歌曲失败，请查看后台或联系管理员")

@Song.got("SelectNumber",prompt="请输入歌曲序号进行点歌：")
async def SelectSong(bot:Bot,state:T_State,SelectNumber:int=ArgPlainText()):
    Songlink = ncm.ncm_song_get(state['Songname'],SelectNumber)
    await Song.finish(MessageSegment.record(Songlink))
    

