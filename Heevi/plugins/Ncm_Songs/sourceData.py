import qrcode.constants
import requests
from pyncm import apis,GetCurrentSession,DumpSessionAsString,LoadSessionFromString
import qrcode
import time
import json
import os
from nonebot import logger
from pydub import AudioSegment
import uuid
import psutil

main_Dir = 'ncm_Dir'

AudioSegment.ffmpeg = r"H:\ffmpeg-2025-03-06-git-696ea1c223-full_build\bin\ffmpeg.exe"  # 替换为你的实际路径
AudioSegment.converter = r"H:\ffmpeg-2025-03-06-git-696ea1c223-full_build\bin\ffmpeg.exe"

sessions_Saveing_Path = os.path.join(main_Dir, 'info_Doc', 'session.json')

if not os.path.exists(main_Dir) :
    os.makedirs(main_Dir)
    logger.success("插件主目录创建成功")

require_Dirs = ['info_Doc', 'download_Doc','convert_Doc']
for dir_name in require_Dirs:
    dir_path = os.path.normpath(os.path.join(main_Dir, dir_name))
    
    if not os.path.exists(dir_path):
        os.makedirs(dir_path, exist_ok=True)  
        if os.path.isdir(dir_path):
            logger.success(f"目录创建成功：{dir_path}")
        else:
            logger.error(f"目录创建失败：{dir_path}")
    else:
        logger.info(f'目录已存在：{dir_path}')



class Ncm():
    def __init__(self):
        self.api = apis


    def login(self) -> bool :
        userUid = apis.login.LoginQrcodeUnikey()['unikey']
        login_Url = f"https://music.163.com/login?codekey={userUid}"
        login_Img = qrcode.QRCode(
            error_correction=qrcode.constants.ERROR_CORRECT_L,
            box_size=1,
            border=1
        )
        login_Img.add_data(login_Url)
        login_Img.make(fit=True)
        #print(login_Img)
        
        logger.info("\n请扫描下方二维码登录：")
        login_Img.print_ascii(invert=True)

        while True :
            for dot in ['...','.. ','.  '] :

                reponse = apis.login.LoginQrcodeCheck(userUid)
                print(f"{reponse['code']}--{reponse['message']}",dot,end='\r')
                if reponse['code'] == 803 or reponse['code'] == 800 :
                    print (f"{reponse['code']}--{reponse['message']}","...")
                    apis.login.WriteLoginInfo(apis.login.GetCurrentLoginStatus(),GetCurrentSession())
                    self.save_User()                  
                    return True
                time.sleep(1)
    
    def load_User(self):
        if os.path.exists(sessions_Saveing_Path):
            with open(sessions_Saveing_Path, 'r', encoding='utf-8') as f:
                session_data = json.load(f)
                LoadSessionFromString(session_data['session_str'])
                logger.info(f"已检测到用户 {GetCurrentSession().nickname} 的登录记录")
        else:
            logger.info("未检测到登录记录，即将进行扫码登录")
            self.login()

    def save_User(self):
        sessions = GetCurrentSession()
        session_data = {
            'cookies': GetCurrentSession().cookies.get_dict(),
            'session_str': DumpSessionAsString(sessions)
        }

        with open(sessions_Saveing_Path, 'w', encoding='utf-8') as f:
            json.dump(session_data, f, indent=2, ensure_ascii=False)

        LoadSessionFromString(session_data['session_str'])

    def search_Song(self,songName:str) -> list:
        response = apis.cloudsearch.GetSearchResult(
            songName,
            stype=1,
            limit=10,
            offset=0)
        if response['code'] == 200:
            logger.success('请求成功，正在搜索歌曲')
            if response['result'] :
                logger.success('搜索成功，正在获取数据')
                Songs = []
                for index, data in enumerate(response['result']['songs'], 1):
                    # 安全访问嵌套字段
                    singer = data.get('ar', [{}])[0].get('name', '未知歌手')
                    album = data.get('al', {}).get('name', '未知专辑')
                    
                    Songs.append({
                        "index": index,
                        "title": data.get("name", "未知歌曲"),
                        "singer": singer,
                        "album": album,
                        "id": data.get("id", 0)
                    })
            else:
                logger.info('未搜索到结果')
        else:
            logger.error(f'请求失败{response['code']}')
        return Songs
    

    def getSongLink(self,songID:str):
        data = self.api.track.GetTrackAudio(songID,bitrate=640000)
        if data['code'] == 200:
            logger.success('请求成功，正在获取歌曲链接')
            songlink = data['data'][0]['url']
            amr_path = self.convertToAmr(songlink)
            return amr_path
        else:
            raise logger.error('请求失败')
    
    def convertToAmr(self, songLink, outputDir='ncm_Dir/convert_Doc',use_wb=True):
        outputDir = os.path.abspath(outputDir).replace('/', os.sep)
        os.makedirs(outputDir, exist_ok=True)

        tmp_filename = f"{uuid.uuid4()}.tmp"
        amr_filename = f"{uuid.uuid4()}.amr"
        tmp_path = os.path.join(outputDir, tmp_filename).replace('/', os.sep)
        output_path = os.path.join(outputDir, amr_filename).replace('/', os.sep)

        try:
            # 下载文件
            response = requests.get(songLink, timeout=30)
            response.raise_for_status()
            with open(tmp_path, 'wb') as f:
                f.write(response.content)
            logger.success(f"歌曲文件已下载: {tmp_path} (大小: {os.path.getsize(tmp_path)} 字节)")

            # 转换
            audio = AudioSegment.from_file(tmp_path)
            try:
                audio = AudioSegment.from_file(tmp_path)  # 不能用 `with` 语句
                audio = audio.set_frame_rate(8000).set_channels(1)
                audio.export(output_path, format='amr', codec='libopencore_amrnb')
                logger.success(f"转换完成: {output_path} (大小: {os.path.getsize(output_path)} 字节)")
            finally:
                # 强制释放资源
                audio._data = None
                del audio

            if not os.path.exists(output_path):
                raise Exception(f"AMR 文件未生成: {output_path}")
            return output_path

        except Exception as e:
            logger.error(f"转换失败: {str(e)}", exc_info=True)
            raise

        finally:
            # 终止残留的 FFmpeg 进程
            self.kill_ffmpeg_processes()
            # 清理临时文件
            max_retries = 10
            for retry in range(max_retries):
                try:
                    if os.path.exists(tmp_path):
                        os.remove(tmp_path)
                        logger.info(f"临时文件已删除: {tmp_path}")
                        break
                except PermissionError:
                    logger.warning(f"文件占用，等待重试 ({retry+1}/{max_retries})")
                    time.sleep(3)
            else:
                logger.error(f"无法删除临时文件: {tmp_path}")
    
    def kill_ffmpeg_processes(self):
        for proc in psutil.process_iter(['name']):
            if proc.name() == 'ffmpeg.exe':
                try:
                    proc.kill()
                    logger.info(f"已终止 FFmpeg 进程: PID {proc.pid}")
                except psutil.NoSuchProcess:
                    pass


    


    
nncm = Ncm()
nncm.load_User()