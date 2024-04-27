from nonebot import on_message
from nonebot.rule import keyword
from nonebot.exception import FinishedException
from nonebot.params import EventPlainText
from nonebot.adapters.onebot.v11 import MessageSegment
from .get_info import get_videoinfo

Bili_Analys = on_message(rule=keyword('bilibili.com/video/'),priority=10,block=True)

@Bili_Analys.handle()
async def Video_Analys(message=EventPlainText()):
    try :
        Cut = message.index('BV')
        BV = message[Cut+2:Cut+12]
        data = get_videoinfo(BV)
        info = data['data']
        Video_Name = info['title']
        Video_Area = info['tname']
        Video_desc = info['desc']
        Video_Oner = info['owner']['name']
        video_data = info['stat']
        Video_view = video_data['view']
        Video_Like = video_data['like']
        Video_Danmu = video_data['danmaku']
        Video_Reply = video_data['reply']
        Video_Favorite = video_data['favorite']
        Video_Coin =video_data['coin']
        Video_Share = video_data['share']
        Video_pic = info['pic']
        await Bili_Analys.finish(MessageSegment.image(Video_pic)+MessageSegment.text("视频标题：{}\t视频分区：{}\nup主：{}\t播放量：{}\n点赞：{}\t投币：{}\n收藏：{}\t评论：{}\n弹幕：{}\t分享：{}\n视频简介：{}".format(Video_Name,Video_Area,Video_Oner,Video_view,Video_Like,Video_Coin,Video_Favorite,Video_Reply,Video_Danmu,Video_Share,Video_desc)))
    except FinishedException :
        raise
    except Exception as e :
        await Bili_Analys.finish("视频解析错误：{}".format(e))


           
