from nonebot import on_command,logger
from typing import Union
from nonebot.rule import to_me
from nonebot.matcher import Matcher
from nonebot.adapters.onebot.v11 import Message,Bot,MessageSegment,GroupMessageEvent,PrivateMessageEvent
from nonebot.params import CommandArg,ArgPlainText
from nonebot.typing import T_State
from nonebot.exception import MatcherException
from .sourceData import nncm as ncm

Song = on_command("点歌", rule=to_me(), priority=10, block=True)

@Song.handle()
async def GetSong(matcher:Matcher,arg:Message = CommandArg()):
    if arg.extract_plain_text():
        matcher.set_arg("SongName",arg)

@Song.got("SongName",prompt="想点哪首歌呢")
async def SearchSong(bot:Bot,state : T_State,SongName=ArgPlainText(),event = Union[GroupMessageEvent,PrivateMessageEvent]):
    try :
        logger.info(SongName)
        SongList = ncm.search_Song(SongName)
        if SongList:
            state['SongList'] = SongList
            logger.success("获取歌曲成功，正在生成选歌菜单")
            Menu_Card = "🎵 点歌菜单 🎵\n"
            for song in SongList :
                Menu_Card += f"{song['index']}.《{song['title']}》 - {song['singer']}\n"
            await bot.send(event=event,message=Menu_Card)
        else :
            await Song.finish("获取歌曲失败，请查看后台或联系管理员")
    except Exception as e :
        logger.error(f"获取歌曲时出现错误，错误为{e}")
        await Song.finish("获取歌曲失败，请查看后台或联系管理员")

@Song.got("SelectNumber",prompt="请输入歌曲序号进行点歌：")
async def SelectSong(
    bot:Bot,
    state:T_State,
    SelectNumber=ArgPlainText()
):
    
    amrPath = ncm.getSongLink(state['SongList'][int(SelectNumber) - 1]['id'])
    await Song.finish(MessageSegment.record(file=amrPath))
    

