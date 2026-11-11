#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
int main(){
  char b[4096];
  ssize_t n=readlink("/proc/self/exe",b,sizeof b-1);
  if(n>=0){b[n]=0;printf("self(n=%zd)=%s\n",n,b);}
  else printf("readlink error: %s\n",strerror(errno));
  return 0;
}
