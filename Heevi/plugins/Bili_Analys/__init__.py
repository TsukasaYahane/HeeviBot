from nonebot import on_message,logger
from nonebot.rule import regex
from nonebot.exception import FinishedException
from nonebot.adapters.onebot.v11 import MessageSegment, Event
from .get_info import get_videoinfo, b23tv_get
import re
import json

def num_handle(num: int) -> str:
    if num > 10000:
        return f"{num / 10000:.2f}万"
    return str(num)

pattern = r'(b23.tv/|bilibili.com/video/|BV[a-zA-Z0-9]{10})'

Bili_Analys = on_message(rule=regex(pattern, flags=re.I), priority=10, block=True)

@Bili_Analys.handle()
async def Video_Analys(event: Event):
    try:
        content = str(event.get_message())
        BV = None
        
        try:
            data = json.loads(content)
            title = data.get('title', '')
            prompt = data.get('prompt', '')
            
            if title == '哔哩哔哩' or '[QQ小程序]' in prompt:
                qqdocurl = data.get('meta', {}).get('detail_1', {}).get('qqdocurl', '')
                if qqdocurl:
                    BV = b23tv_get(qqdocurl)
        except json.JSONDecodeError:
            pass
        
        if not BV:
            url_pattern = re.compile(r"b23\.tv/([a-zA-Z0-9]{7})", re.IGNORECASE)
            urls = url_pattern.findall(content)
            if urls:
                try:
                    url = f"https://b23.tv/{urls[0]}"
                    BV = b23tv_get(url)
                except Exception as e:
                    logger.error(f"视频解析错误：{e}")
                    await Bili_Analys.finish(f"视频解析错误：{e}")
                    
        
        if not BV:
            match = re.search(r'BV[a-zA-Z0-9]{10}', content)
            if match:
                BV = match.group(0)
        
        if not BV:
            await Bili_Analys.finish("未能提取到有效的 BV 号")
            logger.error("未能提取有效BV号")
        
        data = get_videoinfo(BV)
        info = data['data']
        Video_Name = info['title']
        Video_Area = info['tname']
        Video_desc = info['desc']
        Video_Owner = info['owner']['name']
        video_data = info['stat']
        Video_view = num_handle(video_data['view'])
        Video_Like = num_handle(video_data['like'])
        Video_Danmu = num_handle(video_data['danmaku'])
        Video_Reply = num_handle(video_data['reply'])
        Video_Favorite = num_handle(video_data['favorite'])
        Video_Coin = num_handle(video_data['coin'])
        Video_Share = num_handle(video_data['share'])
        Video_pic = info['pic']
        
        await Bili_Analys.finish(
            MessageSegment.image(Video_pic) +
            MessageSegment.text(
                f"【视频标题】：{Video_Name}\n"
                f"【视频分区||up主】：{Video_Area}||{Video_Owner}\n"
                f"【播放量】：{Video_view}\t【点赞】：{Video_Like}\t【投币】：{Video_Coin}\t【收藏】：{Video_Favorite}\n"
                f"【评论】：{Video_Reply}\t【弹幕】：{Video_Danmu}\t【分享】：{Video_Share}\n"
                f"【视频简介】：{Video_desc}"
            )
        )
    except FinishedException:
        raise
    except Exception as e:
        await Bili_Analys.finish(f"视频解析错误：{e}")